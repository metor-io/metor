# Message wiring parity (`message-wiring`)

> **Status: IMPLEMENTED (2026-07-02), then further reframed.** This document's message-wiring-parity
> design landed as WP1-WP9 (`docs/message-wiring-plan.md` — its own status banner lists the shipped
> deviations, chiefly WP7/WP8's command-plane realization). **The command plane it designed (§6,
> §9 item 5, §10 Q7) was then reframed again** by `docs/review-fixes-plan.md` waves 4a-4c
> (landed): `ChannelId`/`channel_id` is deleted (`SequenceCommand`/`SequenceChannelEvent` are
> name-addressed, `channel: String`); the "fan out to every slot, slot self-filters" shape (§9 item
> 5, §10 Q7's "implicit fan-out default") did **not** survive — every command producer × slot pair
> is now an **explicit** message edge, no broadcast sugar; `CommandOut<M>` shipped as a `MsgOut<M>`
> type alias, not a newtype; and the coordinator is an ordinary system #0 (`"coordinator"`), not a
> hand-registered special case. See `docs/messages.md`'s status banner and
> `docs/design-command-slots.md` for the current shape; this document's §1-§5, §7-§8 (the port
> unification itself — kind-tagged `PortDesc`, typed `MsgOut<M>`/`MsgIn<M>`, `AllOutputs`,
> reader-slot self-derivation) remain accurate and are what actually shipped. §6 and the
> command-routing parts of §9/§10 are historical design rationale, superseded as above.
>
> This design promotes messages to first-class **ports** and **edges**, makes the uplink and downlink
> **fully ordinary systems**, and deletes every command-plane special case in the
> coordinator. It builds directly on the *shipped* message channel (`docs/messages.md`
> §1-§7); read that first — this doc changes how messages are *wired*, not their record format,
> ring semantics, or the downlink FIFO.
>
> Decisions 1-7 (`docs/message-wiring.md` §9) were **locked** by the reviewer at design time (item 5
> is superseded, per above). Every NEW fork this design surfaced is collected in **§10 (Open
> questions for review)** — resolutions are recorded there; Q7's resolution (implicit fan-out) is
> likewise superseded.

---

## 1. Overview — the symmetry statement

The framework already moves two payload kinds — **component frames** (fixed `#[repr(C)]`
tables, `docs/frames.md`) and **messages** (`(PacketId, postcard)` byte-ring records,
`docs/messages.md`). Frames are **wired**: a system declares typed `Input<F>`/`Output<F>`
ports in a bundle (`src/system/mod.rs:28-44`), the derive macros enumerate them as
`PortDesc`s (`src/descriptor.rs:36`), and `connect`/KDL edges address them by
`(instance, frame)` and validate/size/allocate at `build()`
(`src/coordinator/mod.rs:790`). Messages are **not**: they are minted coordinator-side
(`msg_writer`, `src/coordinator/mod.rs:1390`) and reach consumers through capabilities and
a hardcoded command bus.

This design closes that gap. After it, a message channel is a **port** with a
`PortDesc`, a message connection is an **edge** with the exact
validate/size/allocate/bind path frames use, and the one structural difference is
recorded honestly in the port kind:

| | **Frame port** (today) | **Message port** (this doc) |
|---|---|---|
| Port types | `Input<F>` / `Output<F>` (`src/port.rs:49,143`) | `MsgIn<M>` / `MsgOut<M>` (`src/message.rs`) |
| Edge key | `frame_id: ComponentId` = `F::FRAME_ID` | `packet_id: PacketId` = `M::ID` |
| KDL address | `connect "a" -> "b" frame="imu"` | `connect "a" -> "b" msg="SequenceCommand"` |
| Compatibility | same `frame_id` **+ component subset** | same `packet_id` **(opaque — no subset)** |
| Input multiplicity | **exactly one** producer | **zero / one / many** producers |
| Ring record | frame table bytes | `(PacketId, postcard)` |
| Cycle detection | participates (break with `connect_delayed`) | **excluded** (decoupled event/command bus) |

The payoff:

- A user wires `producer.msg -> consumer.msg` through the normal wiring system (KDL or
  `WiringBuilder`), in both front-ends, byte-equivalent.
- The **uplink** becomes an ordinary `AsyncSystem` with a typed message output; its
  ground subscription is *derived from its out-edges*.
- **`drain_command_bus`, `CyclicSlot::command`, and `command_sources`** all disappear:
  slots become ordinary `MsgIn<SequenceCommand>` consumers wired by message edges, and the
  coordinator becomes an ordinary command *producer* for `control_handle()`.
- A single reusable **`AllOutputs`** receive-all port generalizes exactly what
  `TelemetryPorts` does today (`src/telemetry/mod.rs:562-584`).

The key symmetry to preserve: *a frame's ring bytes are its wire bytes* (`design.md`) — and
*a message's ring bytes are its wire bytes* — is untouched. Only the **wiring** layer
changes; the data path, the lossless rings, the every-record drain,
and the non-coalescing downlink FIFO (`docs/messages.md` §3.1) are all unchanged.

---

## 2. The typed message port model

### 2.1 `MsgOut<M>` / `MsgIn<M>` become typed on one `Msg`

**[LOCKED 1]** Today `MsgOut` is *type-erased* (`emit<M: Msg>` is generic per call,
`src/message.rs:68,95`) and `MsgIn<M>` is already typed (`src/message.rs:137`). This design
makes **both** typed on one `M`:

```rust
// src/message.rs — sketch
pub struct MsgOut<M, B = BoxBacking, WD = NoWake, WS = NoWake> {
    writer: Writer<B, WD, WS>,
    scratch: Vec<u8>,
    _m: PhantomData<fn() -> M>,
}

impl<M: Msg, B: Backing, WD: WakeSource, WS: WakeSink> MsgOut<M, B, WD, WS> {
    /// Emit one `M`: write `M::ID` then postcard(M) as one record. No per-call generic —
    /// the port carries exactly one Msg type, so the edge can be keyed on `M::ID`.
    pub fn emit(&mut self, msg: &M) -> Result<(), WriteError> { /* as today, M fixed */ }

    /// This port's static descriptor — the message twin of `Output::<F>::descriptor`
    /// (`src/port.rs:74`).
    pub fn descriptor() -> PortDesc { PortDesc::msg::<M>() }
}
```

A channel that carries several `Msg` types becomes **several ports** — one `MsgOut<A>`,
one `MsgOut<B>` — each a distinct edge key. This is a real behavioural change: the sequence
bridge's `"sequences"` channel currently emits both `SequenceRegistry` (coordinator-side,
`src/coordinator/mod.rs:1035`) and `SequenceChannelEvent` (per-slot, `src/coordinator/slot.rs:297`).
In the *current* code these are already on **separate rings** with the same channel name
(`message_entry(instance, "sequences", …)` vs `message_entry("coordinator", "sequences", …)`,
`src/coordinator/mod.rs:987,1027`), each single-type — so the typed split is a near-noop
for the shipped consumers, but it must be stated: a genuinely heterogeneous channel is now
modelled as N typed ports, and `MsgIn::drain`'s id-filter (`src/message.rs:177`) degrades
to belt-and-suspenders (with typed edges every record on a wired channel shares `M::ID`).

*Trade-off (kept as the locked decision):* typed ports buy the edge machinery (one key per
port, `compatible` = a simple id check, sizing per port) at the cost of the "one writer,
many Msg types" ergonomic the type-erased `MsgOut` gave. That ergonomic never had a wired
consumer — telemetry taps every channel regardless of type (`src/telemetry/mod.rs:713`) —
so the loss is only to a hypothetical multi-type single-port emitter, which re-expresses as
N ports.

### 2.2 The `PortDesc` representation — a kind-tagged descriptor

A message port has no `VTable`, no `frame_id`, no `announce` (messages are self-describing,
`docs/messages.md` §1.1 / registry Q3, `src/registry.rs:103-107`). Rather than thread dead
fields or `if kind == Message` branches through the frame-shaped `PortDesc`
(`src/descriptor.rs:36-62`), make `PortDesc` **kind-tagged** but keep it a single struct so
the derive still produces one homogeneous `Vec<PortDesc>` (`src/system/mod.rs:31,43`):

```rust
// src/descriptor.rs — sketch
/// The edge key — a frame id or a message id, in disjoint value spaces so a frame port and
/// a message port can never accidentally match.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortId {
    Frame(ComponentId),   // F::FRAME_ID
    Msg(PacketId),        // M::ID
}

/// The kind-specific payload. Only `Frame` carries the vtable/announce the wiring
/// compatibility check + telemetry announce need; `Message` is self-describing;
/// `ReceiveAll` is the registry tap (§4) that reserves no ring and connects no edge.
pub enum PortKind {
    Frame { vtable: VTable, announce: AnnounceFn },
    Message { telemetered: bool },   // command channels set telemetered=false (Q8, §6.4)
    ReceiveAll,
}

pub struct PortDesc {
    pub id: PortId,           // the edge key
    pub name: &'static str,   // F::NAME / M::SCHEMA.name / "" for ReceiveAll
    pub max_size: usize,      // F::MAX_SIZE / MAX_MSG_BYTES / 0
    pub kind: PortKind,
}

impl PortDesc {
    pub fn of<F: Frame>() -> Self { /* Frame{vtable,announce}, id=Frame(F::FRAME_ID) … */ }
    pub fn msg<M: Msg>() -> Self {
        PortDesc { id: PortId::Msg(M::ID), name: msg_name::<M>(),
                   max_size: MAX_MSG_BYTES, kind: PortKind::Message }
    }
    pub fn receive_all() -> Self { /* id irrelevant, kind = ReceiveAll */ }
}
```

`msg_name::<M>()` is the Msg's postcard-schema name (`M::SCHEMA.name`,
`../metor-proto/src/types.rs:594-595`) — the same string whose fnv1a-16 hash *is* `M::ID`.
This is what makes KDL name-addressing work (§3.4) with no side table.

**Blast radius (honest accounting).** Making `PortDesc` kind-tagged touches every current
reader of `port.frame_id` / `port.vtable` / `port.announce`. Verified sites:
`compatible` (`src/descriptor.rs:149`), `PortDesc::of` (`:90`),
`Input/Output::descriptor` (`src/port.rs:74,169`), `Out::descriptors` push
(`src/system/mod.rs:111-112`), the `build()` port lookups and ring loop
(`src/coordinator/mod.rs:801-816,882-904`, `registry_entry` `:1415`), the slot-aux frame-id
comparisons (`:967,979,1000`, `slot.rs`), and `resolve_endpoint` (`src/wiring/mod.rs:1657`).
These all move from `p.frame_id` to `match p.id { PortId::Frame(f) => …}` / `p.kind`. It is a
mechanical but wide change; §7 lists it. *Trade-off vs a separate `MsgPortDesc` + parallel
`SystemDescriptor.msg_inputs/msg_outputs` lists:* the parallel-list shape would keep
frame code byte-identical but force the **derive macro** to partition fields by kind (it is
type-blind today — it just calls `<#ty>::descriptor()`, `macros/src/system.rs:33`), and
would fork every downstream loop into two. The single kind-tagged `Vec<PortDesc>` keeps the
macro *unchanged* (see §2.3) and the graph algorithms single-pass, at the cost of the
widen-in-place edit. **Recommendation: single kind-tagged `PortDesc`.** **[OPEN Q1]**

`PortRef` (`src/coordinator/mod.rs:117`) widens correspondingly:

```rust
pub struct PortRef { pub system: SystemHandle, pub port: PortId }  // was { system, frame_id }
impl PortRef {
    pub fn new<F: Frame>(system: SystemHandle) -> Self { … PortId::Frame(F::FRAME_ID) }
    pub fn msg<M: Msg>(system: SystemHandle) -> Self  { … PortId::Msg(M::ID) }
}
```

### 2.3 The derive macros need *no change*

`#[derive(SystemInput)]`/`#[derive(SystemOutput)]` emit
`descs.push(<#ty>::descriptor())` per field and `#id: <#ty>::bind(src)` per field
(`macros/src/system.rs:33,52`). Because `MsgOut<M>`/`MsgIn<M>` implement the *same*
`descriptor()`/`bind()` surface `Output<F>`/`Input<F>` do, a message port drops into a
bundle beside frame ports with **zero macro change**:

```rust
#[derive(SystemOutput)]
struct NavOut {
    solution: Output<NavSolution>,      // frame port
    events:   MsgOut<NavEvent>,         // message port — same bundle, same derive
}
```

`descriptors()` yields `[Frame(nav_solution…), Message(nav_event…)]`; `bind()` pops one ring
each in field order. The positional binder (`src/binder.rs:78-225`) is unchanged: a message
ring is an ordinary `RingBuffer<BoxBacking>`, so `next_output`/`next_input` hand it over and
`MsgOut::bind`/`MsgIn::bind` wrap it. This is the crux of the design — **messages ride the
existing bundle/descriptor/binder machinery verbatim once the port types satisfy the port
contract.**

The one wrinkle is a **many-producer** `MsgIn` (§3.2): its `bind` must claim *several*
producer rings, not one. That is handled entirely inside the binder cursor (§3.3), invisible
to the macro.

---

## 3. The message edge model

### 3.1 Addressing

A message edge addresses ports by `(instance, PortId::Msg(M::ID))`, exactly as a frame edge
addresses `(instance, PortId::Frame(F::FRAME_ID))`. The `EdgeSpec` (`src/wiring/model.rs:203`)
is unchanged in *shape* — it already carries `out`/`in_` as **strings** (`"imu"`); for a
message edge those strings are the Msg schema name (`"SequenceCommand"`), resolved to a
`PacketId` at `resolve_endpoint` (§3.4). No new `EdgeSpec` field is required; the port kind
is discovered by looking the name up in the descriptor.

### 3.2 Multiplicity — many-to-many, inputs optional

**[LOCKED 7]** A message input may have **zero, one, or many** producers; a producer may
feed **many** inputs. Concretely, `build()`'s edge pass (`src/coordinator/mod.rs:800-858`)
splits by kind:

| Check (today, frame-only) | Frame ports | Message ports |
|---|---|---|
| `DoubleConnect` — two producers into one input (`:827-835`) | **keep** (exactly one) | **drop** (fan-in allowed) |
| `UnconnectedInput` — input never written (`:849-858`) | **keep** | **drop** (zero producers legal) |
| `compatible()` per edge (`:817`) | subset check | id-equality (§3.5) |
| `FeedbackCycle` over non-delayed edges (`:842`) | **keep** | **exclude** (§3.6) |
| fan-out reader budget (`:861-864`) | keep | keep (generalized, §3.3) |

The consumer-edge map `cons_edge: HashMap<(cons, in_idx), (prod, out_idx)>`
(`src/coordinator/mod.rs:796`) assumes one producer per input (its `insert` returning `Some`
*is* the `DoubleConnect` error). For message inputs it becomes a multimap:
`msg_cons_edges: HashMap<(cons, in_idx), Vec<(prod, out_idx)>>`. Frame inputs keep the
scalar map (and its exactly-once guarantee); message inputs collect the fan-in list.

### 3.3 Ring allocation, binding & reader-slot sizing

**Producer side (unchanged shape).** Each `MsgOut<M>` output port sizes and allocates one
ring, exactly as a frame output does (`src/coordinator/mod.rs:882-904`), but with the message
sizing (`msg_capacity(MAX_MSG_BYTES, MSG_DEPTH)`, `src/message.rs:43`) instead of
`capacity_for(max_size, depth)`. The `max_readers` budget is the **same formula**:

```
max_readers = fan_out(prod, out_idx)      // number of consumer edges on this message output
            + n_registry_consumers        // AllOutputs / downlink taps (§4)
            + READER_SLACK
```

Today message rings are sized `n_reg + READER_SLACK` with **no** fan-out term
(`src/coordinator/mod.rs:985,1024`) because no message edges existed. Adding the fan-out term
generalizes them to wired fan-out — a command channel fanned to every slot needs one reader
slot per slot, which the frame fan-out map (`:861-864`) already computes; we simply feed
message outputs through the same map.

**Consumer side — the fan-in reconciliation.** A frame cyclic input views its single
producer's output ring directly (`src/coordinator/mod.rs:1228`). A message input may have K
producers, so it holds **K views**. The framework splits async decoupling by the **delivery
axis**, not by payload kind (only async *Snapshot* inputs are decoupled through a private
latest-wins copy-in ring, `plan_copy_ins`):

- **Cyclic message consumer → direct multi-view.** `MsgIn<M>` internally holds
  `views: Vec<View>` — one per producer edge — and `drain(f)` drains all of them each cycle
  (per-producer order preserved; cross-producer interleave arbitrary, irrelevant since each
  record self-addresses). This is *exactly* today's `command_sources: Vec<MsgIn<…>>` drain
  (`src/coordinator/mod.rs:1258,1693-1703`) generalized to one port. Same-cycle, no ordering
  constraint, no extra ring. The `SlotRunner` holds precisely this (§6).

- **Async message consumer → the same direct multi-view.** A message input is a Log
  (every-record) input, and Log inputs are **not** copy-in decoupled: the rings are lossless,
  so the async consumer can safely poll-drain the K producer views directly — a slow consumer
  backpressures its producers rather than losing records. There is no private merge ring and
  no `MsgCopyIn`; the private copy-in ring exists only for async **Snapshot** (frame) inputs,
  where latest-wins mirroring decouples the cycle from the consumer.

To bind K rings positionally, the binder cursor gains a per-input **fan-in list**. `BoundPort`
for an input becomes:

```rust
// src/binder.rs — sketch
enum BoundInput {
    One(BoundPort),          // a frame input (exactly one producer) or a 1-producer msg input
    Many(Vec<BoundPort>),    // a fanned-in message input (K producer rings)
}
```

and the `RingSource` grows one method the `MsgIn::bind` uses:

```rust
trait RingSource {
    // … next_output / next_input unchanged (frame ports) …
    /// Pop every producer ring wired to the next (message) input port. `Vec::new()` is a
    /// legal, unconnected message input (reads nothing). Frame ports never call this.
    fn next_input_fanin<RD, RS>(&mut self) -> Vec<(RingBuffer<Self::B>, RD, RS)> { … }
}
```

`Output<F>::bind`/`Input<F>::bind` are untouched (they call `next_output`/`next_input`);
`MsgIn<M>::bind` calls `next_input_fanin`; `MsgOut<M>::bind` calls `next_output`. Positional
alignment holds because the coordinator lays out one `BoundInput` per input **port** (in
`descriptors()` order) — `One` for frame ports, `Many` (possibly empty) for message ports.

*Trade-off (direct multi-view vs a uniform merge ring for all consumers):* a uniform merge
ring would make every message input a single-view port (simplest bundle story) but forces a
copy + a head-of-cycle ordering constraint (the merge must run *before* the consumer steps)
and an extra ring per input. Direct multi-view avoids both and matches the cyclic frame path.
**Shipped shape: direct multi-view for every message consumer, cyclic and async alike** — the
lossless rings make the async case safe without a merge ring (Q2's original "async = merge
ring" recommendation did not survive).

### 3.4 KDL + `WiringBuilder` surface

Both front-ends must express message edges byte-equivalently (they already share one
`Wiring` model, `design.md` "Wiring a mission"). The surfaces mirror `frame=`:

**KDL** — add a `msg=` property to `connect`, parsed beside `frame=` in `parse_edge`
(`src/wiring/mod.rs:1590-1634`). Exactly one of `frame=`/`msg=` is required:

```kdl
connect "uplink" -> "adcs" msg="SequenceCommand"      // message edge
connect "coordinator" -> "adcs" msg="SequenceCommand" // in-proc emitter → slot (§6)
connect "imu" -> "nav" frame="imu"                    // unchanged
```

`parse_edge` currently stores the frame name into both `out` and `in_`
(`src/wiring/mod.rs:1630-1631`); for `msg=` it stores the Msg name the same way, and records
the endpoint *kind* so `resolve_endpoint` knows which port list to search. Cleanest: keep
`EdgeSpec` as strings and let `resolve_endpoint` (`:1639`) try the name as **both** a frame
name and a Msg name against the descriptor's ports, disambiguated by which of `frame=`/`msg=`
the KDL used. **[OPEN Q3]** (whether to add an explicit `kind` discriminant to `EdgeSpec` vs
inferring it at resolve — recommend a small `EdgeSpec.kind: EdgeKind { Frame, Msg }` for
clarity and a precise `UnknownFrame`/`UnknownMsg` diagnostic).

`resolve_endpoint` maps a Msg name to a `PacketId` with the **same hash the `Msg` derive
uses** — `fnv1a_hash_str_16_xor(name).to_le_bytes()` (`../metor-proto/src/types.rs:595`) — so
`msg="SequenceCommand"` yields exactly `SequenceCommand::ID`, then matches it against the
instance descriptor's `PortId::Msg` ports (a typo is an `UnknownMsg` load error, parallel to
`UnknownFrame`, `:1658`). This is the message twin of `ComponentId::new(frame)` (`:1652`).

**`WiringBuilder`** — add `connect_msg` / `connect_msg_delayed` beside `connect`
(`src/wiring/builder.rs:128,147`), and low-level `CoordinatorBuilder::connect_msg(PortRef,
PortRef)` beside `connect` (`src/coordinator/mod.rs:743`). Both push an `EdgeSpec`/edge with
the message kind. The typed builder path stays: `PortRef::msg::<SequenceCommand>(handle)`.

### 3.5 Compatibility rule

**[LOCKED 1]** Messages are opaque postcard with no component structure, so there is **no
subset relation**. `compatible` for two message ports is pure `PacketId` equality:

```rust
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool {
    match (&producer.kind, &consumer.kind) {
        (PortKind::Frame { vtable: p, .. }, PortKind::Frame { vtable: c, .. })
            => producer.id == consumer.id && subset(c, p),   // today's rule, src/descriptor.rs:149
        (PortKind::Message, PortKind::Message) => producer.id == consumer.id,
        _ => false,                                          // frame↔message never compatible
    }
}
```

Because `PortId::Msg(M::ID)` already encodes the Msg identity, id-equality *is* type-equality:
a `MsgOut<A>` → `MsgIn<B>` edge with `A::ID != B::ID` is an `Incompatible` build error
(`src/coordinator/mod.rs:821`). Stricter than frames (no forward-compat), which is the honest
consequence of opaqueness — a consumer cannot ignore "extra fields" of a postcard blob it
cannot parse. Noted as a deliberate asymmetry (§9, decision 1).

### 3.6 Cycle-detection stance

**[OPEN Q4 → recommend]** Frame feedback cycles are rejected unless one edge is
`connect_delayed` (`src/coordinator/mod.rs:836,842`), because a frame edge is a synchronous
same-cycle data dependency. A message edge is **not**: it is a decoupled event/command bus,
read every-record with no same-cycle production guarantee — a
command loop (slot → autonomy system → command → slot) is legitimate and common. **Recommend:
message edges are excluded from cycle detection entirely** (they simply do not populate
`forward_adj`, `:836-838`), exactly as `delayed` frame edges are. This means a message edge
*cannot* create a `FeedbackCycle` and never needs `connect_delayed`. Flagged for confirmation
because it is a genuine new policy (not one of the seven locked decisions).

---

## 4. The receive-all tap port (`AllOutputs`)

**[LOCKED 3]** Today `TelemetryPorts` hand-writes a bundle that pulls **both** registries in
`bind` (`src/telemetry/mod.rs:562-584`) — `output_registry()` for frames,
`message_registry()` for messages — and `add_telemetry` manually bumps
`n_registry_consumers` (`src/coordinator/mod.rs:600`) so every ring reserves it a reader slot.
Generalize that into one reusable port any bundle can declare:

```rust
// src/registry.rs (or a new src/tap.rs) — sketch
/// A broadcast tap over EVERY output frame AND message channel in the graph. The reusable
/// generalization of TelemetryPorts (src/telemetry/mod.rs:562). Reserves no ring and
/// connects no edge; it is a *capability* port, but it appears in a bundle like any port.
pub struct AllOutputs {
    pub outputs:  Arc<OutputRegistry>,
    pub messages: Arc<MessageRegistry>,
}
impl AllOutputs {
    pub fn descriptor() -> PortDesc { PortDesc::receive_all() }   // kind = ReceiveAll
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        Self { outputs: src.output_registry(), messages: src.message_registry() }
    }
}
```

**How it appears in a bundle.** As an ordinary field:
`struct TelemetryPorts { all: AllOutputs }` (derived) — the derive calls
`AllOutputs::descriptor()` and `AllOutputs::bind(src)` like any port, **no macro change**.

**Untelemetered channels are skipped.** `AllOutputs` taps every *telemetered* message channel;
a `MessageEntry` with `telemetered == false` (the command channels, Q8/§6.4) is skipped by the
tap loop, so receive-all never re-ingests command traffic. Frame outputs are always telemetered.

**How build() treats a `ReceiveAll` PortDesc.** It is neither sized nor edge-validated. In
the ring loop (`src/coordinator/mod.rs:882`) and the `BoundPort` layout (`:1216-1231`),
`ReceiveAll` ports are **skipped** (they allocate no ring, so they create no `BoundPort`, so
the positional cursor never hands one out — `AllOutputs::bind` pops nothing, keeping
alignment). In the edge pass they are never a valid endpoint (they carry no `PortId`).

**No edge in wiring.** A receive-all port needs *no* `connect` — it taps the whole graph
implicitly, like the `All` telemetry mode does today. There is nothing to declare in KDL.

**Reader-slot budgeting generalizes.** Every `ReceiveAll` port is one extra reader on **every**
output *and* message ring. So `n_registry_consumers` is no longer manually bumped by
`add_telemetry`; instead `build()` **counts `ReceiveAll` PortDescs across all systems'
descriptors** and that count *is* `n_reg`. This deletes the `n_registry_consumers += 1` line
(`src/coordinator/mod.rs:600`) and makes the budget self-derived: any system that declares
`AllOutputs` automatically gets a reader slot on every buffer. (The `add_telemetry`
convenience remains, but as a plain `add_cyclic_named("telemetry", …)` with no manual bump.)

**Re-express the downlink on it.** `TelemetryPorts::bind` (`src/telemetry/mod.rs:578-583`)
becomes `Self { all: AllOutputs::bind(src) }`, and `init` reads `output.all.outputs` /
`output.all.messages` (`:676-677`). This proves the generalization and removes the bespoke
double-pull. **Recommendation: yes, re-express telemetry on `AllOutputs`.** The bespoke
`output_registry()`/`message_registry()` capabilities on `RingSource` (`src/binder.rs:138,147`)
stay (they are what `AllOutputs::bind` calls); only their *caller* changes.

---

## 5. The uplink as a normal system

The uplink is already an ordinary `AsyncSystem` (`UplinkSystem<R>`, `src/telemetry/mod.rs:452`)
with an `UplinkPorts { commands: MsgOut }` output bound via `src.command_out()`
(`:409-427`). Two changes make it fully wired:

### 5.1 Its command output becomes a normal typed message port

`UplinkPorts` today pulls the bespoke `command_out()` capability (`src/binder.rs:158`), which
mints a ring into a side collector (`command_rings`, `:87`) that the coordinator drains
out-of-band. Under parity it declares a **normal** typed output:

```rust
#[derive(SystemOutput)]
struct UplinkPorts { commands: MsgOut<SequenceCommand> }   // an ordinary edge-addressable output
```

Its ring is allocated/sized/bound like any message output (§3.3), and consumers (slots) wire
message edges to it. The `command_out()` capability and the `command_rings` collector
(`src/binder.rs:83-88,151-160,212-224`) are **deleted** — there is no side channel any more.

### 5.2 The ground subscription is derived from out-edges — the "output defined by consumers" inversion

**[LOCKED 2]** The uplink must tell the ground link *which* Msg ids to subscribe to
(`TcpRecvTransport::ensure` sends `MsgStream { msg_id }`, hardcoded to `SequenceCommand::ID`
today, `src/telemetry/mod.rs:211-220`). Under parity that set is exactly the Msg ids the
uplink *emits onto wired out-edges*. The uplink learns its own out-edge id set through a new
bind-time capability — the same pattern as `output_registry()`:

```rust
trait RingSource {
    /// The PacketIds of the message OUTPUT ports of the system currently being bound that
    /// have at least one out-edge. The "output defined by consumers" inversion: the uplink
    /// subscribes on the ground to exactly what downstream systems wired out of it.
    fn out_msg_ids(&self) -> Vec<PacketId> { Vec::new() }
}
```

The host `Binder` is constructed *per system* in the bind loop
(`src/coordinator/mod.rs:1233`); it already knows the system index `id`, and after edge
resolution it knows every edge — so it carries this system's out-edge Msg ids and returns them
here. `UplinkPorts::bind` stashes them in a non-ring field (like `TelemetryPorts` stashes the
registries):

```rust
struct UplinkPorts { commands: MsgOut<SequenceCommand>, subscribe: Vec<PacketId> }
impl BindPorts<BoxBacking> for UplinkPorts {
    fn bind<S: RingSource<B = BoxBacking>>(src: &mut S) -> Self {
        Self { commands: MsgOut::bind(src), subscribe: src.out_msg_ids() }
    }
}
```

`UplinkSystem::run` passes `output.subscribe` to the transport once, before the first `recv`.
`RecvTransport` grows a `subscribe(&mut self, ids: &[PacketId])` (default no-op for the mock);
`TcpRecvTransport::ensure` (`src/telemetry/mod.rs:205-225`) sends one `MsgStream { msg_id }` per
id instead of the hardcoded `SequenceCommand::ID`. A future uplink that also forwards another
command type just declares a second `MsgOut<OtherCmd>` port and wires it — the subscription set
grows automatically, no code change.

*Fork — edge-derived vs declared-port-derived.* The simpler equivalent is "subscribe to the
PacketIds of the uplink's declared message-output ports" (read straight off its
`SystemDescriptor.outputs`), needing no new capability. The locked decision says *out-edges*,
which additionally **prunes** a declared-but-unwired output from the subscription (don't
subscribe to a command type nothing consumes). Recommend implementing the edge-derived form via
`out_msg_ids()` as above; if the pruning is not wanted, the declared-port form is a two-line
simplification. **[OPEN Q5]**

### 5.3 What changes vs today

- `UplinkPorts` output goes from bespoke `command_out()` to a normal `MsgOut<SequenceCommand>`
  edge output (§5.1); it gains the `subscribe` field (§5.2).
- `add_uplink` (`src/coordinator/mod.rs:614`) stays a thin `add_async` — but callers/resolvers
  now must also **wire** the uplink's `commands` output to the slots (or rely on the
  command-fan-out convenience, §6.3). This is the one new wiring obligation the uplink gains
  by becoming normal.
- Everything else (its own connection, drop-on-disconnect, the `run` filter,
  `src/telemetry/mod.rs:470-493`) is unchanged.

---

## 6. The command-plane reframe

**[LOCKED 4,5,6]** Delete the coordinator's hardcoded command bus; express command delivery as
ordinary message edges.

### 6.1 What is deleted

| Deleted (file:line) | Replaced by |
|---|---|
| `fn drain_command_bus` (`src/coordinator/mod.rs:1693-1703`) | slots draining their own `MsgIn<SequenceCommand>` in `step` (§6.2) |
| `self.drain_command_bus()` call (`:1614`) | — (nothing; slots self-drain) |
| `CyclicSlot::command` trait method + default no-op (`:263-268`) | ordinary `MsgIn` drain inside `SlotRunner::step` |
| the broadcast loop `for cmd … for slot … slot.command(cmd)` (`:1698-1702`) | fan-out is the *edges*; filtering is the slot's `channel_id` check |
| `command_sources: Vec<MsgIn<SequenceCommand>>` field + build (`:1258-1276,1465`) | the slots' own inputs |
| the coordinator-owned `command_ring` allocated ad-hoc (`:1045-1051,1460`) | a normal `coordinator.commands` message output (§6.3) |
| `RingSource::command_out` + `command_rings` collector (`src/binder.rs:83-88,151-160,212-224`) | normal `MsgOut` output ports + message edges |

### 6.2 Slots as wired `MsgIn<SequenceCommand>` consumers (host-side)

**[LOCKED 5 + assumption]** The slot's *registered descriptor* (`src/coordinator/mod.rs:712`)
gains one **message input port** `MsgIn<SequenceCommand>` (name `"commands"`), so an edge can
target it. But the `MsgIn` is owned **host-side by the `SlotRunner`**, exactly as its
`events: MsgOut` / `seq_status: Input` are today (`src/coordinator/slot.rs:201-208`) — the
occupant stays unaware, **zero ABI change** (`docs/messages.md` §6, confirmed: the occupant's
`fsw_*` surface is untouched). Mechanically:

- `SlotAux` (`src/coordinator/mod.rs:422`) gains `commands: MsgIn<SequenceCommand>` — a
  multi-view `MsgIn` (§3.3) over every producer ring wired to this slot's command input
  (the coordinator's `coordinator.commands` and the uplink's `uplink.commands`, plus any user
  emitter). The build loop claims those views from the resolved fan-in list.
- `SlotRunner::new` takes it; `SlotRunner` holds it (drops `channel_id`-only comment plumbing
  is unchanged — it already has `channel_id`, `slot.rs:208`).
- `SlotRunner::step` (`slot.rs:452`) drains it at the **head of the step**, before polling the
  occupant, and dispatches each command through the existing handlers — preserving the
  same-cycle property (a command lands the cycle it arrives):

```rust
fn step(&mut self, now: Timestamp) {
    self.last_now = now;
    // was: coordinator.drain_command_bus() → slot.command(cmd); now the slot self-drains.
    let mut cmds = Vec::new();
    self.commands.drain(|c| cmds.push(c));      // multi-view drain, §3.3
    for cmd in cmds { self.apply_command(&cmd); }   // the body of today's SlotRunner::command
    self.publish_status(now);
    // … unchanged occupant poll …
}
```

`apply_command` is verbatim today's `SlotRunner::command` (`slot.rs:515-526`): the
`cmd.channel_id == self.channel_id` filter and the `SequenceCommandKind` → `do_load/…/do_reset`
match are **unchanged**. Because a message edge fans the command channel out to *every* slot
(§6.3) and each slot filters by `channel_id`, behaviour is **identical** to today's
broadcast-then-filter — just expressed as edges + a per-slot drain instead of a central
broadcast. `channel_id` assignment (the build-order index among slots, `:948`) and the boot
`SequenceRegistry` (`:1035,5`) are untouched.

*Note:* the `CyclicSlot::command` trait method disappears; the slot's command handling is now
private to `SlotRunner`. Non-slot cyclic systems that want commands declare their own
`MsgIn<M>` bundle port (the general parity path) — they are no longer implicitly part of a
broadcast.

### 6.3 The coordinator as a normal command *producer* (without a bundle)

**[LOCKED 6]** `control_handle()` must keep returning a `MsgOut<SequenceCommand>`
(`src/coordinator/mod.rs:1554`) on the *same wired channel* the slots read — so the in-proc
path and the uplink path are one mechanism. The subtlety: the coordinator has **no
descriptor/bundle**, so it cannot go through `add_*` + `bind`. The mechanism:

The coordinator is modelled as a **reserved pseudo-instance `"coordinator"`** owning exactly
one message output port `commands` (`MsgOut<SequenceCommand>`). Its ring is hand-allocated at
`build()` (as `command_ring` is today, `:1045`) rather than bundle-bound, and it is seeded into
the producer/fan-out tables **before** slots are wired, so:

- `PortRef::msg::<SequenceCommand>(COORDINATOR_HANDLE)` is a valid edge source, addressable in
  KDL as instance `"coordinator"` (the same reserved name coordinator-owned buffers already
  downlink under, `COORDINATOR_INSTANCE`, `:1410`).
- `control_handle()` mints a `MsgOut<SequenceCommand>` over that ring, unchanged (`:1554`).
- The ring's `max_readers` is sized to the slot fan-out + `n_reg` + slack via the same fan-out
  map as any producer.

This is the **one irreducible asymmetry**: the coordinator's command output is
*hand-registered* (one ring, one `MsgOut`, one synthetic descriptor entry) rather than
bundle-bound, because it is not a system. It is small and self-contained — a single reserved
producer — and it keeps `control_handle()` and the uplink path *the same message channel*
(decision 6's goal). *Alternative considered:* a full synthetic `SystemDescriptor` +
`Reg::Coordinator` variant that binds through the normal loop — cleaner conceptually but forces
the coordinator to fabricate a bundle for one port; the hand-registered producer is less code
and no less honest. **Recommendation: hand-registered reserved producer.** **[OPEN Q6]**

**Command fan-out — implicit vs explicit.** **[OPEN Q7]** Decision 5 says commands "fan out to
all slots." Two ways to realize the edges:

- *Explicit:* the user writes `connect "coordinator" -> "adcs" msg="SequenceCommand"` and
  `connect "uplink" -> "adcs" msg="SequenceCommand"` per slot. Fully first-class, but N_slots ×
  N_emitters lines of boilerplate for what is almost always "every command emitter → every
  slot."
- *Implicit convenience:* the framework auto-adds, at `build()`, an edge from every declared
  `MsgOut<SequenceCommand>` (the coordinator's + the uplink's + any emitter's) to every slot's
  `commands` input — expanding to the same ordinary edges. Preserves "it's just edges"
  underneath while sparing the boilerplate.

**Recommendation: default to the implicit fan-out convenience** (it reproduces today's
zero-wiring command delivery and matches the `SequenceCommand`-broadcast mental model), with
explicit `connect … msg=` available for a constrained command topology (e.g. an autonomy
system that should command only one slot). Flagged because it is a real ergonomics/first-class
tension the reviewer should rule on.

### 6.4 Are command channels telemetered?

**[RESOLVED Q8 — keep commands OFF the downlink]** Today the command channels are deliberately
kept **out** of the `MessageRegistry` (not downlinked) so uplinked commands are not echoed
straight back on the downlink (`src/coordinator/mod.rs:1039-1044,1256`). Under parity, an
ordinary `MsgOut` output would land in the `MessageRegistry` and be tapped by
`AllOutputs`/telemetry (§4) — echoing every uplinked command back to the panel and doubling
command traffic on a constrained link. **Decision: preserve today's exclusion via a per-port
`telemetered` flag.** `PortKind::Message { telemetered: bool }` (§2.2) defaults `true` for a
user message output; `SequenceCommand` command outputs (the coordinator's reserved producer and
the uplink's `commands`) declare `telemetered = false`. Mechanically:

- The flag rides the `PortDesc` into the `MessageEntry` (`src/registry.rs:108`) as a
  `telemetered: bool` field.
- `AllOutputs`/telemetry's message tap (`src/telemetry/mod.rs:709-728`) and any future
  registry consumer **skip** entries with `telemetered == false`, so an untelemetered channel is
  still a first-class wired port (fan-in/fan-out, sized, bound) but is never downlinked.
- How the uplink/coordinator command ports set the flag: the typed `MsgOut<M>` port carries no
  such knob by default (a plain `MsgOut<NavEvent>` is telemetered). The command producers opt
  out through a marker — cleanest is a thin `CommandOut<M>` newtype whose `descriptor()` returns
  `PortDesc::msg_untelemetered::<M>()`, used by `UplinkPorts.commands` and the reserved
  coordinator producer. **[plan detail]** the exact opt-out spelling (newtype vs a const on the
  port vs a builder flag) is left to the plan; the data-model decision — a `telemetered` bool on
  the message `PortDesc`/`MessageEntry`, default true, false for commands — is fixed here.

This keeps commands as inbound control (as today) while everything else about them is ordinary
wired message ports. A user message channel between two systems telemeters normally.

### 6.5 The updated cycle

```
cycle N:
  ├─ (async message merge copy-ins for async command consumers, if any)
  ├─ step slots / cyclic systems
  │     └─ SlotRunner::step: drain own MsgIn<SequenceCommand> (multi-view) → apply by channel_id
  │        → poll occupant   (command applies THIS cycle — same-cycle preserved)
  ├─ frame async copy-in
  └─ telemetry (downlink) snapshot         (registered last; AllOutputs taps every ring)
```

The head-of-cycle `drain_command_bus` stage is gone; each slot drains its own input at the head
of its own step. Same-cycle latency is preserved because a slot steps after the coordinator has
already run the producers'… — note: the coordinator's `control_handle` emits happen *between*
cycles (host/CLI/test call sites), and the uplink fills its ring out-of-band (async), so both
producers' records are present when the slot drains at the head of its step. The boot
`SequenceRegistry` emit (`:1542,1589`) is unchanged.

---

## 7. Migration / impact

**ABI impact: none** (at the time of this wave). Every change was host-side
(`docs/messages.md` §6 confirmed against the code: occupant `fsw_*`, `FSW_ABI_VERSION`, and
`RawBinder` were untouched). Message rings remain plain byte rings; the slot's new `MsgIn` is
host-owned in `SlotRunner`.

> **Superseded by the port unification (A1/C7,** `docs/design-port-unification.md` **§7):**
> `PortDescMsg` is now schema-tagged (`PortSchemaMsg::Table | Postcard`) and carries the
> behavior axes + `telemetered`, so a dlopen'd system **can** declare `MsgOut`/`MsgIn` ports —
> `RawBinder` binds them positionally like any port (the dl fixture exercises a Postcard
> output end-to-end). Only the host *capabilities* (`AllOutputs`/`ReceiveAll`, the registry
> pull) remain host-only: a `.so` declaring one is rejected at load.

### 7.1 Deletions (with file:line) and replacements

| Deleted | file:line | Replaced by |
|---|---|---|
| `drain_command_bus` + its call | `src/coordinator/mod.rs:1693-1703,1614` | per-slot `MsgIn` drain (§6.2) |
| `CyclicSlot::command` method + no-op default | `src/coordinator/mod.rs:263-268` | `SlotRunner`-private `apply_command` (§6.2) |
| `command_sources` field + build + accessor use | `src/coordinator/mod.rs:1258-1276,1465` | the slots' own command inputs |
| ad-hoc `command_ring` alloc | `src/coordinator/mod.rs:1045-1051,1305,1460` | reserved `coordinator.commands` producer (§6.3) |
| `RingSource::command_out` + host impl | `src/binder.rs:151-160,212-224` | normal `MsgOut<SequenceCommand>` output |
| `command_rings` collector + Binder field | `src/binder.rs:83-88,96,110,222,238` (and the coordinator `&mut command_rings` thread, `:1110,1238`) | edge fan-in from wired producers |
| `add_telemetry`'s `n_registry_consumers += 1` | `src/coordinator/mod.rs:600` | count `ReceiveAll` PortDescs (§4) |
| bespoke `TelemetryPorts` double-pull | `src/telemetry/mod.rs:573-584` | `AllOutputs` (§4) |
| hardcoded `MsgStream { SequenceCommand::ID }` | `src/telemetry/mod.rs:211-220` | `subscribe(&out_msg_ids)` (§5.2) |
| `MsgOut` type-erasure (`emit<M>`) | `src/message.rs:95` | `MsgOut<M>::emit(&M)` (§2.1) |

### 7.2 Additions

- `PortId` / `PortKind` / kind-tagged `PortDesc` + `PortDesc::msg`/`receive_all`
  (`src/descriptor.rs`); `PortRef.port` (`src/coordinator/mod.rs`).
- `MsgOut<M>` typed; `MsgOut<M>::descriptor`/`bind`, `MsgIn<M>::descriptor`, multi-view
  `MsgIn` (`src/message.rs`).
- `AllOutputs` port (`src/registry.rs` or new `src/tap.rs`).
- `RingSource::next_input_fanin` + `out_msg_ids`; `BoundInput` fan-in (`src/binder.rs`).
- ~~`MsgCopyIn` for async message inputs~~ — not needed: async message consumers hold the
  direct fan-in views (§3.3); only async Snapshot (frame) inputs are copy-in decoupled.
- `CoordinatorBuilder::connect_msg`/`connect_msg_delayed`; `WiringBuilder::connect_msg`;
  `EdgeSpec` kind + KDL `msg=` (`src/wiring/*`).
- `compatible` message arm; `build()` kind-split edge pass (§3.2); reserved coordinator
  producer + optional implicit command fan-out (§6.3).
- `RecvTransport::subscribe`; per-id `MsgStream` loop (`src/telemetry/mod.rs`).

### 7.3 Example + tests

- **`examples/cube-sat/src/main.rs`** is a hand-written **ground/panel-side** loop (it
  `TcpStream::connect`s to the db at `2240` and subscribes to streams, `:541-560`; grep
  confirms it uses none of the coordinator/builder/`add_*` API). It is unaffected by the
  FSW-side wiring change **except** the two-connection consequence already noted in
  `docs/messages.md` §7 (the panel must open a second connection for the uplink). No new change
  from *this* refactor beyond keeping its `MsgStream { SequenceCommand::ID }` subscribe
  (`:555`) — which still matches the id the FSW uplink now derives from its out-edges.
- **`tests/slot_integration.rs`** is the primary test to update. It drives slots through
  `coord.control_handle()` + `coord.channel_id("adcs")` + `control.emit(&load(ch,…))`
  (`:184-187,244-248,300-303,353-360,430-433`). Under the reframe these APIs are **unchanged**
  (`control_handle` still returns a `MsgOut<SequenceCommand>`, `channel_id` still resolves the
  build-order index), and the coordinator now delivers via the reserved `coordinator.commands`
  producer auto-fanned to the slot (§6.3) — so the tests should pass with **no source change**
  if the implicit command fan-out (Q7) is the default. If explicit fan-out is chosen, the
  tests (and `resolve`) must add the `coordinator -> slot msg=` edges. The event/registry drain
  assertions (`:404-483`) and the mock-uplink test (`:483-516`) exercise the unchanged
  downlink/uplink record path.
- **`src/telemetry/tests.rs:424-444`** constructs a bare `MsgOut` (type-erased) and emits two
  Msg types through it — it must switch to a typed `MsgOut<M>` per type (§2.1).
- **`src/message.rs` unit tests** (`:230-340`) similarly emit two Msg types through one
  `MsgOut`; split into typed ports.
- New tests: a two-system user message edge (producer `MsgOut<E>` → consumer `MsgIn<E>`,
  cyclic multi-view fan-in), a fan-in of two emitters into one slot, `AllOutputs` on a non-
  telemetry system, and the `msg=` KDL round-trip (parse → resolve → build).

---

## 8. Reconciled tensions (the hard parts, summarized)

- **Type-erased `MsgOut` → typed ports.** Reconciled by splitting a heterogeneous channel into
  N typed ports (§2.1); the shipped consumers were already single-type on separate rings.
- **Many-to-many messages vs exactly-once frames.** Reconciled by kind-splitting the `build()`
  edge pass: frame inputs keep the scalar `cons_edge` + exactly-once/no-double checks; message
  inputs use a fan-in multimap and drop those checks; consumer-side fan-in is direct multi-view
  (cyclic) or a single-writer merge ring (async) — reusing the copy-in idea without violating
  single-writer discipline (§3.2-§3.3).
- **The coordinator-as-producer without a bundle.** Reconciled by a reserved, hand-registered
  `coordinator.commands` producer (one ring, one `MsgOut`), the single small asymmetry, keeping
  `control_handle()` and the uplink on one channel (§6.3).
- **A receive-all "port" that reserves no ring and connects no edge.** Reconciled by a third
  `PortKind::ReceiveAll` that `build()` skips for sizing/edges and counts for the reader budget,
  so `AllOutputs` is a normal derived field with self-derived `n_reg` (§4).
- **KDL name → PacketId.** Reconciled for free: `M::ID` *is* `fnv1a16(M::SCHEMA.name)`
  (`../metor-proto/src/types.rs:594-595`), so `msg="SequenceCommand"` hashes to the exact id,
  parallel to `frame="imu"` → `ComponentId` (§3.4).

---

## 9. Resolved decisions

The seven reviewer-locked decisions, restated with trade-offs, plus the new forks this design
raised (each cross-referenced to its **[OPEN Qn]** in §10).

1. **Typed single-`Msg` ports — LOCKED.** `MsgOut<M>`/`MsgIn<M>` typed on one `M`; edge key
   `M::ID`; a multi-type channel is N ports; compatibility is pure `PacketId` equality (no
   subset — opaque postcard). *Trade-off:* loses the "one writer, many Msg types" ergonomic
   (no wired consumer needed it) and forgoes frame-style forward-compat (a consumer cannot
   ignore extra fields of a blob it cannot parse). (§2.1, §3.5.)

2. **Uplink subscription derived from out-edges — LOCKED.** A new `RingSource::out_msg_ids()`
   pulled in `UplinkPorts::bind` hands the uplink the PacketIds wired out of it; it subscribes
   exactly those on the ground (`MsgStream` per id). *Trade-off:* a new per-system bind
   capability vs the simpler declared-port-derived set; edge-derived additionally prunes
   unwired outputs. (§5.2, **[OPEN Q5]**.)

3. **Receive-all `AllOutputs` (frames + messages) — LOCKED.** A reusable `PortKind::ReceiveAll`
   port binding both registries; ring-less, edge-less, self-counting for `n_reg`; telemetry's
   downlink re-expresses on it. *Trade-off:* a third port kind threaded through `build()` vs
   the bespoke per-consumer double-pull; the third kind pays for itself by auto-deriving the
   reader budget. (§4.)

4. **Reframe the command bus — LOCKED.** `drain_command_bus`, `CyclicSlot::command`, the
   broadcast, and `command_sources` are deleted; command delivery is ordinary message edges.
   *Trade-off:* the central bus was one place to reason about command timing; now timing is a
   property of edges + the slot's head-of-step drain (same-cycle preserved). (§6.)

5. **Command routing = fan-out to all slots, slot self-filters by `channel_id` — LOCKED, then
   SUPERSEDED.** As designed: behaviour identical to today's broadcast-then-filter, expressed as
   edges + the unchanged `cmd.channel_id == self.channel_id` filter. *As shipped
   (`docs/review-fixes-plan.md` wave 4a/4c):* `channel_id` is gone (name-addressed, `cmd.channel ==
   self.name`), and the "implicit fan-out convenience" below (§10 Q7) was **not** kept — every
   producer × slot pair needing a command edge is wired explicitly in KDL/the builder (no broadcast
   sugar, "fan-out connect sugar out of scope v1" per the review's executive call). (§6.2, **[OPEN
   Q7]**, superseded.)

6. **In-proc control: coordinator is a normal command emitter — LOCKED.** A reserved,
   hand-registered `coordinator.commands` producer (one ring/`MsgOut`) keeps `control_handle()`
   on the same wired channel as the uplink. *Trade-off:* one irreducible asymmetry (a
   hand-registered producer, not a bundle-bound one) vs fabricating a full synthetic system
   descriptor for one port. (§6.3, **[OPEN Q6]**.)

7. **Message edges are many-to-many, inputs optional — LOCKED.** `build()` drops exactly-once
   + unconnected-input + double-connect for message inputs (keeps them for frames); reader-slot
   sizing reuses the frame fan-out formula; feedback-cycle detection *excludes* message edges.
   *Trade-off:* a fanned-in message ring needs enough reader slots (handled by the fan-out map);
   excluding message edges from cycle detection permits command loops with no `connect_delayed`.
   (§3.2, §3.6, **[OPEN Q4]**.)

**New forks (recommendations flagged for review) —** Q1 (`PortDesc` shape), Q2 (cyclic
multi-view vs merge ring), Q3 (`EdgeSpec` kind discriminant), Q4 (message cycle exclusion), Q5
(edge- vs declared-port-derived subscription), Q6 (reserved producer vs synthetic descriptor),
Q7 (implicit vs explicit command fan-out), Q8 (are command channels telemetered). Each is in
§10.

---

## 10. Open questions for review

**ALL RESOLVED (reviewer, 2026-06-30).** Q1–Q6, Q9, Q10 confirmed as the recommended answer
below. **Q7 = implicit fan-out default + explicit `connect … msg=` override** — as designed;
**superseded as shipped**: the review's later command-plane pass (`docs/review-fixes-plan.md` wave
4c) dropped the implicit-fan-out default entirely in favor of always-explicit command edges (no
broadcast sugar; see the §9 item 5 update above). **Q8 REVERSED from the recommendation: keep
command channels OFF the downlink** via a `telemetered` bool on the port descriptor/registry entry
(default true; false for the coordinator + uplink command outputs, via the `CommandOut<M>`
`.untelemetered()` sugar) — this part shipped as designed. Original analysis retained below for
context.

1. **[Q1] `PortDesc` shape.** Widen the existing struct to a single **kind-tagged `PortDesc`
   (`PortId` + `PortKind`)** — keeps the derive macro unchanged and graph algorithms single-pass,
   at the cost of a wide but mechanical edit to every `port.frame_id`/`vtable` reader (§2.2). The
   alternative (a separate `MsgPortDesc` + parallel `SystemDescriptor` lists) keeps frame code
   byte-identical but forks the macro and every loop. **Recommend: kind-tagged single struct.**

2. **[Q2] Fan-in delivery.** **Cyclic message consumers = direct multi-view; async = single-
   writer merge ring** (§3.3). Alternative: a uniform merge ring for all consumers (simpler
   bundle story, extra ring + head-of-cycle ordering constraint). **Recommend: split by driver
   kind.**

3. **[Q3] `EdgeSpec` kind.** Add an explicit **`EdgeSpec.kind: EdgeKind { Frame, Msg }`** for a
   precise `UnknownFrame`/`UnknownMsg` diagnostic and unambiguous resolution, vs inferring the
   kind at `resolve_endpoint` from whether `frame=`/`msg=` was used (§3.4). **Recommend: explicit
   kind field.**

4. **[Q4] Message cycle detection.** **Exclude message edges from feedback-cycle detection
   entirely** (a message channel is a decoupled `Overwrite` bus; command loops are legitimate and
   need no `connect_delayed`) (§3.6). **Recommend: exclude.**

5. **[Q5] Uplink subscription source.** **Edge-derived via `out_msg_ids()`** (prunes unwired
   outputs), vs the simpler declared-output-port set (§5.2). The locked decision 2 says
   out-edges. **Recommend: edge-derived.**

6. **[Q6] Coordinator producer mechanism.** **Reserved hand-registered `coordinator.commands`
   producer** (one ring/`MsgOut`, the single asymmetry) vs a full synthetic `SystemDescriptor` +
   `Reg::Coordinator` that binds through the normal loop (§6.3). **Recommend: hand-registered
   producer.**

7. **[Q7] Command fan-out.** **Default to an implicit "every command emitter → every slot"
   fan-out convenience** (reproduces today's zero-wiring command delivery; keeps
   `tests/slot_integration.rs` source-unchanged), with explicit `connect … msg=` available for a
   constrained topology, vs requiring explicit edges always (§6.3). **Recommend: implicit default
   + explicit override.**

8. **[Q8] Telemetering command channels.** Under parity an ordinary command `MsgOut` lands in the
   `MessageRegistry` and is downlinked — **echoing uplinked commands back to the panel**, unlike
   today's deliberate exclusion (`src/coordinator/mod.rs:1039-1044`). **Recommend: accept the echo
   for v-next** (a confirmed command stream is useful; simplest, full parity), noting an
   "untelemetered output port" flag (`PortKind::Message { telemetered: bool }`) as future work if
   noisy. Confirm — this is a behaviour change (§6.4).

Two smaller confirmations:

9. **[Q9] `AllOutputs` bundle side.** Telemetry keeps it in the **output** bundle (its `init`
   reaches it via `output`, matching today, `src/telemetry/mod.rs:675`); a pure consumer could
   put it in the input bundle. Since `ReceiveAll` is edge-less either way, this is cosmetic.
   **Recommend: allow either; telemetry stays output-side.**

10. **[Q10] `MsgIn::drain` id-filter.** With typed edges every record on a wired channel already
    shares `M::ID`, so the `id == M::ID` skip (`src/message.rs:177`) becomes redundant. **Recommend:
    keep it** (belt-and-suspenders; harmless, and it keeps `MsgIn` usable on a hand-fed
    heterogeneous ring in tests).
