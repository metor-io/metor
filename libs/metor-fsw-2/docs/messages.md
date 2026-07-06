# Messages (`messages`)

> **Status update (2026-07-02): messages are first-class WIRED PORTS, the command plane has been
> REFRAMED, and it has since been REFRAMED AGAIN — see `docs/message-wiring.md` for the wiring-parity
> design, and `docs/review-fixes-plan.md` waves 4a-4c for the final shape (landed).** §1.1, §1.3, §2.1,
> §3, §5, §6 (the record format, the downlink, the sequence coupling, and the ABI note) remain
> accurate in substance. **§1.2/§1.4, §2.2, and all of §4 are SUPERSEDED — twice over**, as follows.
>
> **Normalization update (2026-07-05, `docs/normalize-telemetry-uplink-plan.md`, landed):** the
> uplink and the telemetry downlink are now **registry built-ins** (`type="TcpUplink"` /
> `type="TcpDownlink"`) declared as ordinary `system` nodes; the dedicated `uplink { … }` KDL
> node and `CoordinatorBuilder::add_uplink`/`add_telemetry` were deleted (register with
> `add_async`/`add_cyclic` directly). §4's system design is otherwise as landed.
>
> **First supersession (`docs/message-wiring.md`, landed):**
> - `MsgOut` is now **typed** (`MsgOut<M>`) and, with `MsgIn<M>`, is a first-class port that drops
>   into a `SystemInput`/`SystemOutput` bundle and is wired by an ordinary edge — one `connect` KDL
>   node, kind (frame vs message) inferred from whether `frame=` or `msg=` is present — keyed on
>   `M::ID`. A heterogeneous channel is modelled as N typed ports. (The low-level
>   `CoordinatorBuilder::connect`/`connect_delayed` need only one `connect`, inferring the kind from
>   the `PortRef`'s `PortId::{Component,Packet}`; the higher-level `WiringBuilder` keeps
>   `connect_msg` as ergonomic sugar over the same edge — it was **not** deleted, contra an earlier
>   draft of this note.)
> - §2.2's separate `MessageRegistry` is **gone**: there is now **one** `Registry` for both frames
>   and messages (`EntrySchema::{Table, Postcard}`, `src/registry.rs`), so a same-instance
>   frame/channel name collision is detectable (`WireError::DuplicateRegistryKey`) instead of
>   shadowed across two tables. `AllOutputs` is a `Capability`, not a "receive-all pseudo-port."
>
> **Second supersession (`docs/review-fixes-plan.md` waves 4a-4c, landed 2026-07-02) — the command
> plane described just below is ALSO gone:**
> - `ChannelId`/`channel_id` (a `u64` build-order index) is **deleted**. `SequenceCommand` /
>   `SequenceChannelEvent` carry `channel: String` and are addressed by the slot's **instance
>   name** end-to-end — matching how the panel and the wiring document already name a slot.
> - There is **no coordinator-collected fan-out "by type"** and no coordinator-side drain/dispatch
>   stage at all. Each slot declares an ordinary `commands: MsgIn<SequenceCommand>` **fan-in** port
>   (`FanIn::Many`); every command producer that should reach a given slot is wired to it by an
>   **explicit edge** (`connect "uplink" -> "mode" msg="SequenceCommand"`,
>   `connect "coordinator" -> "mode" msg="SequenceCommand"`) — there is no implicit
>   broadcast-to-every-slot sugar (the "fan out, self-filter" shape below did not survive). At the
>   head of its own `step`, a slot drains its fan-in and applies commands whose `channel` equals its
>   own name (`SlotRunner::apply_command`, `src/coordinator/slot.rs`).
> - `CommandOut<M>` is **not a distinct type**: it is `pub type CommandOut<M, ...> = MsgOut<M, ...>`
>   sugar the `SystemOutput` derive/`#[system]` macro recognizes and lowers to an
>   `.untelemetered()` `PortDesc` (`src/message.rs`) — so inbound control is not echoed on the
>   downlink, without a bespoke capability.
> - The coordinator is registered as an ordinary system **#0** under the reserved instance name
>   `"coordinator"` (`docs/design-command-slots.md` §2.6); `control_handle()` returns a take-once
>   `Option<MsgOut<SequenceCommand>>` over that bundle's own `commands` output, wired like any other
>   producer — not a hand-rolled ring outside the descriptor system.
> - The uplink routes each received wire `Msg` to whichever of its declared `CommandOut<M>` outputs'
>   `PacketId` matches (`RouteMsg::route`, multi-output dispatch), rather than forwarding a single
>   command type pass-through; its ground subscription is still derived from its declared outputs.
>
> **Third supersession (the backpressure refactor, landed): the lap doctrine is gone.** The ring
> is now **lossless-only** — the `Overrun` enum (Overwrite/Lossless) is deleted, a writer can never
> overwrite unread data (`try_write` returns `WouldBlock` when a slow reader is in the way; async
> `write` suspends), and a lap is impossible. Treat every mention below of a lap, a resyncing view,
> `Overrun::Overwrite`, or a best-effort message log as historical: a full message ring
> **backpressures the emitter** — `MsgOut::publish` counts a failed write into the
> `publish_dropped` health error — and a committed record is never lost to a reader. Reads are
> zero-copy in-place borrows (no scratch decode buffer), and `FswStatus` no longer has a
> `StoppedLapped` code (`Running`/`Panicked`/`Done`).
>
> Everything below (§1–§8) is retained as the historical WP11 + redesign narrative; **read
> `docs/message-wiring.md` and this banner for the wiring-parity shape, and treat every mention of
> `channel_id`, a coordinator-drained command bus, or an implicit fan-out below as historical, not
> current.**
>
> ---

> **Status: v1 IMPLEMENTED; command plane being REDESIGNED.** The message channel + downlink +
> sequence coupling shipped across WP11 (W1 message channel → W2 downlink → W4 sequence coupling →
> W3 uplink → W5 example e2e; plan `messages-plan.md`) and remain as described (§1–§3, §5, §6).
> **What this revision supersedes is the *command plane*.** WP11 shipped a `SlotCommand`-Frame
> control plane: the panel published wire `SequenceCommand` Msgs; an `UplinkLauncher` (not a
> system — no descriptor, no ports, not in the graph) fed an `Arc<MsgInbox>`; and the coordinator's
> head-of-cycle `drain_uplink` translated each `SequenceCommand → SlotCommand` (a parallel
> `zerocopy` Frame type) by resolving `channel_id → slot name` through a `channel_map`, then wrote
> it to a `SlotCommand` Frame ring `drain_commands` dispatched. Three faults drove the redesign:
> the coordinator is hard-linked to and aware of sequences/inbox/channel-resolution (it should
> drain a generic command bus and know nothing of sequences); the uplink is a special
> coordinator appendage rather than a normal system (the downlink is an ordinary system — the
> uplink should be too); and uplink+downlink are forced to share one split socket because a
> connection is an owned resource the ring model can't distribute.
>
> **This revision (§4) collapses the two command types to one and re-homes the plane.** The
> internal command plane becomes a **`SequenceCommand` message channel** (postcard on a byte ring),
> end-to-end: `SlotCommand`/`SlotCommandKind` and the `channel_map` *translation* are deleted,
> `CyclicSlot::command` takes `&SequenceCommand`, and `SlotRunner` filters by `channel_id`. The
> uplink becomes a normal `AsyncSystem` (the read twin of the telemetry downlink) that re-emits
> commands onto a per-emitter command channel; the coordinator drains every command channel (each
> emitter's, plus its own in-proc one) each cycle and dispatches by `channel_id` — knowing nothing
> of sequences.
> Uplink and downlink now use **separate connections** (shared connection is punted, §4.5). This
> discharges the previously-deferred in-FSW subscriber `MsgIn<M>` (§1.4). Resolved decisions for
> the redesign are §8 entries 7–11 (and Q5/Q7 are revised in place).
>
> ---
>
> A second, parallel payload kind for the
> framework: **messages** — arbitrary `serde` types (`metor_proto::types::Msg`, `const ID:
> PacketId`) serialized with postcard to variable-length bytes, carried on **byte rings** beside
> the fixed component frames, tapped by telemetry, and downlinked as `OwnedPacket::Msg { id,
> bytes }`. The motivating consumer is the panel's **sequences view**, populated by
> `metor_proto_wkt` Msg packets (`SequenceRegistry` / `SequenceChannelEvent`), **not** by
> component tables — so the slots/sequences feature (`sequences-slots.md`) currently leaves it
> empty. This doc adds the general message channel, taps it in telemetry (the **downlink**), adds
> a symmetric **uplink** system (§4.4) that ingests panel→FSW `SequenceCommand` Msgs and re-emits
> them onto a generic **command bus** (§4.3), and uses both to make the panel's sequence view
> fully interactive. Every fork is resolved (§8); the command plane was redesigned after WP11
> (§4, §8 entries 7–11), and the uplink+downlink shared-connection question (old Q7) is now
> **punted** to separate connections (§4.5).

The framework today moves exactly one kind of payload: **component frames** — fixed
`#[repr(C)]` tables described by a `VTable`, written to rings as their own bytes, tapped by
telemetry, and sent as metor-proto `Table` packets (`system.md`, `telemetry.md`). That path is
load-bearing and unchanged here. Messages are a **second** payload kind that rides the same
ring/registry/telemetry machinery but with a different record shape and a different wire packet.

The two are deliberately distinct (do not conflate them):

| | **Component frame** (today) | **Message** (this doc) |
|---|---|---|
| Type | `#[repr(C)]` `Frame` + `VTable` | any `serde` + `Msg` (`const ID: PacketId`) |
| Ring record | the frame's fixed table bytes | `(PacketId, postcard bytes)` |
| Self-describing | no — needs a `VTable` announce | yes — `PacketId` identifies it on the wire |
| Wire packet | `OwnedPacket::Table` (`LenPacket::table`) | `OwnedPacket::Msg` (`LenPacket::msg`) |
| Telemetry drain | **latest-wins** (snapshot, may coalesce) | **every-record** (log, must not coalesce) |
| Port | `Output<F>` (`src/port.rs`) | `MsgOut` (new) |
| Registry | `OutputRegistry` (`src/registry.rs`) | `MessageRegistry` (new) |

The good news up front: `metor-proto-wkt` is **already** a non-optional dependency
(`Cargo.toml:15`), so `SequenceRegistry`/`SequenceChannelEvent`/`SequenceCommand`
(`../metor-proto/wkt/src/msgs.rs:681,725,753`) are usable directly — no re-declaring the Msgs
locally, and no `--no-default-features` concern (wkt is in the abi-only build already). The ring
already carries arbitrary-length byte records (`RingBuffer::try_write(&[u8])`,
`ring/src/lib.rs:859`), and the telemetry `Transport::send` already takes any `LenPacket`
(`src/telemetry/mod.rs:70,132`) — a `Msg` packet differs from a `Table` packet only by one type
byte (`LenPacket::msg` vs `LenPacket::table`, `../metor-proto/src/types.rs:652,662`). So the
**downlink** is mostly wiring a new record shape through existing machinery. The **uplink** is
the one genuinely new surface — the current `Transport` is write-only (`announce`/`send`), so
ingesting commands needs a read path (§4).

---

## 1. The message abstraction

### 1.1 The record format

One message record on a ring is:

```text
┌──────────────┬─────────────────────────────┐
│ PacketId [2] │ postcard(payload)  (var len) │   ← one ring record (ring/src/lib.rs:859)
└──────────────┴─────────────────────────────┘
```

`PacketId` is `[u8; 2]` (`../metor-proto/src/types.rs:554`). The 2-byte id prefixes the postcard
payload so a tap can split it back out without a side table — the record is **self-describing**,
exactly as the wire `OwnedPacket::Msg { id, bytes }` is (`../metor-proto/src/types.rs:761`,
`MsgBuf { id, bytes }` at `:853`). There is **no** `VTable`, no announce, no schema on the ring.

### 1.2 The emit port — `MsgOut`

A system holds a `MsgOut` the way it holds an `Output<F>` — but `MsgOut` is **type-erased over
the Msg type**, because one channel commonly emits several Msg types (the sequence bridge emits
both `SequenceRegistry` and `SequenceChannelEvent` on one channel). It writes any `Msg`:

```rust
/// One owned message output: the single `Writer` into a byte ring carrying
/// `(PacketId, postcard)` records. Type-erased — accepts any `Msg` — unlike the
/// per-frame `Output<F>` (src/port.rs). Rings are backing-erased, so the same
/// port type serves a (future) dl occupant binding over a raw attach (§6).
pub struct MsgOut<WD = NoWake, WS = NoWake> {
    writer: Writer<WD, WS>,
    scratch: Vec<u8>,             // reused encode buffer (no per-emit malloc)
}

impl<WD: WakeSource, WS: WakeSink> MsgOut<WD, WS> {
    /// Serialize `msg` (id prefix + postcard) into the reused scratch and write it as
    /// one ring record. Variable length — sizing is a heuristic, not `F::MAX_SIZE` (§2).
    pub fn emit<M: Msg>(&mut self, msg: &M) -> Result<(), WriteError> {
        self.scratch.clear();
        self.scratch.extend_from_slice(&M::ID);
        postcard::serialize_with_flavor(msg, /* into self.scratch */ ..)?;
        self.writer.try_write(&self.scratch)
    }
}
```

This is a **general** capability: ANY host system can hold a `MsgOut` and emit subscribable
messages (Q4). A typed convenience newtype `Emitter<M>` (one fixed `M::ID`, `emit(&M)`) can wrap
`MsgOut` for single-type channels, but `MsgOut` is the primitive. Both differ from `Output<F>`
in three ways: variable record length (no `F::MAX_SIZE`), a serialize step (component frames
write their bytes verbatim — `Output::write` is a bare `try_write(frame.as_bytes())`,
`src/port.rs:105`), and no `VTable`/announce.

### 1.3 The subscribe side

Two in-FSW consumers read message rings: **telemetry** (§3), which taps *every* channel and
downlinks it, and — added by the command-plane redesign (§4) — the **coordinator**, which drains
the command channel(s) each cycle. On the wire, the panel is the third subscriber: it matches
incoming `OwnedPacket::Msg` by `id` (the "catch-all pub/sub" — unmatched wkt Msgs are recorded as
telemetry, which is precisely how the sequence view is fed). So "messages that can be subscribed
to" is satisfied by *telemetry subscribing to all of them*, the wire being a pub/sub bus, and the
coordinator subscribing to the command channel(s). The symmetric **ingest** of panel-published
Msgs (commands) back into the FSW is the uplink (§4.4).

### 1.4 The subscribe port — `MsgIn<M>`

The general in-FSW subscriber — a system *reading* another channel's messages — was deferred in
WP11 (it was the §7 "general in-FSW message **subscriber**" item). **The command-plane redesign
(§4) builds it**, because the command bus *is* an in-FSW message reader: the coordinator drains
`SequenceCommand`s off command channels each cycle.

`MsgIn<M>` is the **decode twin of `MsgOut`**. Where `MsgOut::emit` prefixes `M::ID` + postcards
the payload into one ring record, `MsgIn::drain` does the inverse over a read `View`: drain every
record, `split_record` off the 2-byte `PacketId` (`src/message.rs:46`), and postcard-decode the
rest to `M`. It is **typed** (one `M`), unlike the type-erased `MsgOut` — a subscriber names the
one message type it consumes:

```rust
/// One owned message subscriber: a `View` over a byte ring carrying `(PacketId, postcard)`
/// records, decoding each to `M`. The decode twin of `MsgOut` (`src/message.rs`) and the
/// message analogue of `Input<F>` — but typed over one `M` (a reader knows what it consumes),
/// where the emit port is erased (one channel carries many Msg types).
pub struct MsgIn<M> {
    view: View<NoWake, NoWake>,
    scratch: Vec<u8>,                 // reused decode buffer
    _marker: PhantomData<M>,
}

impl<M: Msg + DeserializeOwned> MsgIn<M> {
    /// Drain every record since the last call, decoding each to `M` (records whose id is
    /// not `M::ID` are skipped — a command channel carries only `M`, but the filter keeps
    /// it total). Every-record, never coalesced — a command stream cannot drop. A lapped
    /// view resyncs (§3.1's hazard, mirrored on the read side).
    pub fn drain(&mut self, mut f: impl FnMut(M)) -> Result<(), LapError> { /* … */ }
}
```

This mirrors the every-record `Input::drain` (`src/port.rs:229`) the telemetry message tap and
the `SlotRunner`'s `SequenceStatus` drain already use — `MsgIn` is the message-ring spelling of
the same "drain a `View`, don't coalesce" loop. The coordinator holds one `MsgIn<SequenceCommand>`
per command channel (§4.3).

---

## 2. Message rings + registration

### 2.1 Allocation & sizing

A message ring is an ordinary (lossless) heap-backed `RingBuffer`, allocated by
the coordinator at `build()` alongside the output rings (`src/coordinator/mod.rs:842-867`). The
only new wrinkle is **sizing**: records are variable, so there is no `capacity_for(F::MAX_SIZE,
depth)`. The heuristic:

```rust
// capacity = next_pow2( frame_len(max_msg_bytes) * depth )
//   max_msg_bytes : per-channel config, default 4 KiB (covers a SequenceRegistry
//                   with a realistic channel count; truncation is a config error)
//   depth         : default 64 — generous, because a message ring is an EVENT/COMMAND
//                   LOG, not a latest-wins snapshot; bursts of events between two
//                   telemetry drains must not fill the ring.
```

Because the ring is lossless and bounded, a sufficiently large burst does not lap an un-drained
reader — it **backpressures the emitter** instead: `try_write` returns `WouldBlock`, which
`MsgOut::publish` counts into the `publish_dropped` health error. A committed record is never
lost; readers drain **every cycle** (§3), and generous depth keeps the backpressure path off the
steady state.

### 2.2 The `MessageRegistry`

A parallel index to `OutputRegistry` (`src/registry.rs`), because the registry's per-entry
fields (`vtable`, `metadata`, prefixed `announce`) are meaningless for a self-describing message
ring (Q3):

```rust
pub struct MessageEntry {
    pub instance: Arc<str>,                       // owning system instance (subset filtering)
    pub channel: Arc<str>,                        // the message-channel name (telemetry id)
    pub(crate) ring: RingBuffer,                  // view()-only, slot-accounted (registry.rs:47)
}
pub struct MessageRegistry { entries: Vec<MessageEntry>, /* by_key */ }
```

Like `OutputRegistry`, it hands out `view()`s (slot-accounted against `max_readers`), never the
raw ring. The coordinator freezes it next to `OutputRegistry` before the bind loop
(`src/coordinator/mod.rs:942`) so a consumer can pull it in `BindPorts::bind`.

### 2.3 How a system declares its message outputs

Statically, like ports. A `MsgOut` is declared in the system's `SystemOutput` bundle and bound
positionally by the binder, so sizing/allocation/registration happen at `build()` from the
declared channels — no dynamic registration. The general port, registry, and telemetry tap are
all built in v1 (Q4); the **KDL surface** for user-declared per-system message channels is
deferred. The channels *wired* so far are the host-side sequence bridge's `"sequences"` (§5) and —
added by the command-plane redesign — the `"commands"` channel each command emitter declares (§4.3),
both allocated/registered through the same path (the uplink declares its `"commands"` `MsgOut` in
its output bundle; the coordinator mints its own for `control_handle()`).

---

## 3. Telemetry subscribes to all messages (the downlink)

The `TelemetrySystem` (`src/telemetry/mod.rs`) gains a second tap set parallel to its output
taps. On `init` (`:372`) it walks the `MessageRegistry` and claims a `View` per message ring
(`mode.matches` reused). Each cycle it drains those views and downlinks every record.

### 3.1 The non-coalescing hazard (the load-bearing detail, Q6)

The existing output hand-off **coalesces** — one `Option<LenPacket>` slot per tap, latest-wins,
a newer snapshot overwrites an older un-sent one (`HandOff`, `src/telemetry/mod.rs:182-228`,
`push` at `:211`). That is correct for component snapshots (the panel wants the freshest sample)
but **wrong for messages**: a sequence event log or command stream cannot drop a record — losing
a `Started` between two `Progress` lines corrupts the panel's per-channel state machine.

So messages get a **separate, non-coalescing hand-off**:

```rust
/// FIFO message hand-off: every record is queued and sent in order. No latest-wins
/// slot. Bounded; on overflow it drops the OLDEST and bumps `dropped` (loss is
/// surfaced, never silent reordering). Contrast HandOff (output, latest-wins).
struct MsgHandOff {
    queue: Mutex<VecDeque<LenPacket>>,    // bounded; not Vec<Option<_>>
    dropped: AtomicU64,
    pending: AtomicBool,
    wq: WaitQueue,
}
```

In-cycle `execute` drains **every** record from each message view (per-record in-place read
grants, like `Input::drain` — not the latest-wins borrow the output
taps use), splits the 2-byte id, builds `LenPacket::msg(id, payload.len())`
(`../metor-proto/src/types.rs:652`), extends it with the payload, and pushes each to the FIFO in
ring order. The ring is lossless, so the view never misses a record; the only loss point is the
bounded FIFO hand-off itself (dropped-oldest, surfaced as `telemetry.msg_dropped`).

### 3.2 The send path — no announce

The async sender (`run_sender`, `src/telemetry/mod.rs:240`) drains the message FIFO and calls
`Transport::send(pkt)` — which already accepts any `LenPacket` (`:70,132`) and writes it through
`PacketSink` verbatim, type byte and all. **No announce** for messages: they are self-describing,
so the `announce` step (`:63`, a `VTableMsg` + metadata, output-only) is skipped entirely. The
only change to the *send* surface is none — `send` is payload-kind-agnostic today; we just feed
it `LenPacket::msg(...)` instead of `LenPacket::table(...)`. Ordering: per channel, FIFO drain
preserves the producer's record order; across channels the interleave is arbitrary (and
irrelevant — each Msg carries its own `channel_id`).

---

## 4. The command plane (panel → FSW)

The downlink makes the panel's sequence view *visible*; the command plane makes it *interactive*.
The panel publishes `SequenceCommand { channel_id, command }` Msgs (`msgs.rs:753`) to load/start/
stop/abort/reset channels. This section is the **redesign** of the WP11 command plane (status
banner). It has four moving parts: **one command type** (§4.1), the **`MsgIn` drain** (§1.4), a
**generic command bus** the coordinator fans in (§4.3), and the **uplink as a normal async
system** (§4.4) — plus the decision to use **separate connections** (§4.5).

### 4.1 One command type — collapse to `SequenceCommand` (Q8)

WP11 carried **two** command types and a translation between them:

| | WP11 `SlotCommand` (deleted) | `SequenceCommand` (the survivor) |
|---|---|---|
| Where | `src/coordinator/slot.rs:150` | `metor-proto-wkt`, `msgs.rs:753` |
| Plane | internal control plane | the wire (panel publishes it) |
| Encoding | `zerocopy` fixed-shape Frame | `serde`/postcard, variable length |
| Addressed by | slot **name** string | `channel_id` (`ChannelId` = `u64`, `msgs.rs:662`) |
| Kinds | Load/Start/Stop/Abort/Reset/**Unload** | Load/Start/Abort/Stop/Reset |
| Ring | a `SlotCommand` Frame ring | a `SequenceCommand` **message** channel |

The `channel_map` (`Vec<(ChannelId, &'static str)>`, `mod.rs:1548`) existed **solely** to translate
`channel_id → slot name` on the command path, because the two types addressed differently. That is
all incidental complexity. **Decision: collapse to one `SequenceCommand` end-to-end.** The internal
command plane becomes a postcard message channel carrying the *real* wire `SequenceCommand`,
addressed by `channel_id`; `SlotCommand`/`SlotCommandKind` and the `channel_map` *translation* are
deleted.

**Why a single Rust type cannot literally ride a Frame ring** (the reason WP11 minted a parallel
type rather than reusing the wkt one): a component-frame ring carries `zerocopy`/`#[repr(C)]`
fixed-size records, but `SequenceCommand` is `serde`/postcard *variable-length* (its `Load { name }`
carries a `String`). The two are different serialization domains; you cannot put a postcard type on
a zerocopy ring. So the collapse is not "use the same type on the same ring" — it is **move the
command plane off the zerocopy Frame ring onto a postcard message channel** (§1, §2), where the
wire type rides natively. This is exactly the message channel WP11 already built for the downlink;
the command plane is its second consumer, in the ingest direction.

**`CyclicSlot::command` now takes `&SequenceCommand`.** The coordinator broadcasts each drained
command to every cyclic slot (the existing fan-out, `mod.rs:271`); the default trait no-op makes
non-slots ignore it, and `SlotRunner::command` (`slot.rs:638`) filters by **`cmd.channel_id ==
self.channel_id`** instead of by name (`SlotRunner` already carries its own `channel_id`,
`slot.rs:324`). The kind match maps `SequenceCommandKind` directly onto the existing
`do_load`/`do_start`/`do_abort`/`do_stop`/`do_reset` handlers — no intermediate type:

| wkt `SequenceCommandKind` (`msgs.rs:738`) | `SlotRunner` handler (`slot.rs`) |
|---|---|
| `Load { name }` | `do_load(&name)` |
| `Start` | `do_start()` |
| `Abort` | `do_abort()` |
| `Stop` | `do_stop()` |
| `Reset` | `do_reset()` |

**`Unload` — DECIDED: DROP it (Q9).** `SlotCommandKind::Unload` and `SlotRunner::do_unload`
(`slot.rs:496`, emitting `SequenceEventKind::Unloaded`) are **dead today** — no `SequenceCommand`
ever maps to `Unload` (the wkt `SequenceCommandKind` has no `Unload` variant, `msgs.rs:738`), so the
WP11 `drain_uplink` could never produce one and only an in-proc `control_handle()` test could.
Collapsing to `SequenceCommand` makes `Unload` **unreachable from any command source**. Recommend
**removing** `do_unload` (and the dead `SlotCommandKind::Unload`). `SequenceCommandKind` itself is
**unchanged** (no wire/ABI change). The only loss is in-proc `Unload` parity for a host driving
slots directly; that parity is not wanted (the panel can't express it and `Reset` covers re-arm).
If in-proc `Unload` is ever needed, it returns as a host-only method on the coordinator, not a
command-plane kind — keeping the command vocabulary equal to the wire's.

### 4.2 The coordinator drains the bus — `MsgIn`, same cycle

The coordinator drains the command channel(s) at the **head of the cycle** with one
`MsgIn<SequenceCommand>` (§1.4) per channel, **before** stepping slots — so a command dispatches the
*same* cycle it lands (no off-by-one). This is the message-ring spelling of the old `drain_commands`
(`mod.rs:1755`): drain → collect → broadcast to every `CyclicSlot::command`. The WP11
`drain_uplink` translation stage (`mod.rs:1714`, `SequenceCommand → SlotCommand` via `channel_map`)
**disappears entirely** — there is nothing to translate; the drained `SequenceCommand` is dispatched
directly.

### 4.3 The generic command bus + the emitter capability (Q10)

The coordinator should know **nothing** about sequences, inboxes, or channel resolution — it should
just *drain a command bus and dispatch*. The bus is the ingest twin of telemetry's read-all output
tap (§3): where the downlink taps **every** output ring, the command bus drains **every command
channel** an emitter declared.

**Shape — DECIDED: per-emitter command channels, collected at bind (not one shared bus).** Any
command-emitting system asks for a command channel through the bind-time emit capability
(`RingSource::command_out`, below); the coordinator mints a fresh `SequenceCommand` message ring per
ask and **collects it directly** into a builder-local list (`command_rings`, `mod.rs`). After the
bind loop it claims one `MsgIn<SequenceCommand>` `View` per collected channel (slot-accounted), plus
one for its own in-proc channel. Each cycle it `drain`s them all and dispatches each decoded
`SequenceCommand` to the slots. This supports **multiple** command emitters (an uplink, a
scripted-test emitter, a future autonomy system) with no contention — each gets its own
single-writer ring — and emitters declare themselves just by pulling the capability.

The command channels are deliberately **coordinator-internal**: they are drained directly via the
collected `MsgIn` set, *not* registered in the downlink `MessageRegistry`. Commands are **inbound
control, not telemetry**, so keeping them out of the registry (a) avoids echoing every uplinked
command straight back onto the downlink, and (b) sidesteps the registry-freeze ordering (the
`MessageRegistry` is frozen *before* the bind loop, but command channels are allocated *during* it
as systems pull the capability). This is the one deliberate deviation from a pure
"tap-the-registry" symmetry with the downlink; everything else mirrors §3.

**The coordinator also keeps ONE owned command channel** for the in-proc `control_handle()`
(`mod.rs`): a coordinator-owned `SequenceCommand` message ring whose `MsgIn` is the first entry in
the drain set, and over which `control_handle()` mints a `MsgOut` the host/CLI/tests `emit` through.
So `control_handle()` returns a `MsgOut<SequenceCommand>` instead of an `Output<SlotCommand>`; the
in-proc path and the uplink path are now *the same mechanism* (a `SequenceCommand` message channel
drained by `MsgIn`), not two. A test/CLI addresses a slot by `channel_id` — resolved once from its
name via the new `Coordinator::channel_id(name)` helper.

**The bind-time emitter capability** mirrors `output_registry()`/`message_registry()` on
`RingSource`/`Binder` (`src/binder.rs`): `command_out()` allocates a fresh command ring, appends it
to the coordinator's `command_rings` collector, and returns the single `MsgOut` over it. A system
that emits commands pulls it in `BindPorts::bind` exactly where it pulls its typed ports — i.e. it
declares a `MsgOut` in its output bundle (the `UplinkPorts` bundle does exactly this), narrowing the
general `MsgOut` user-bundle path deferred in WP11 Q4 to this one capability. Only the host `Binder`
carries it; any non-host (dl) source panics, as the registries do.

**Updated cycle diagram** (replaces the WP11 `drain_inbox → drain SlotCommand` stages):

```text
   cycle N:
     ├─ drain command bus     ← drain every emitter's MsgIn<SequenceCommand> (collected at bind)
     │                          + the coordinator's own control_handle() channel;
     │                          dispatch each by channel_id to CyclicSlot::command  (BEFORE steps)
     ├─ step slots / cyclic systems   (SlotRunner acts on the command this same cycle)
     ├─ async copy-in
     └─ telemetry (downlink) snapshot          (registered last)
```

The same-cycle property is unchanged from WP11: the command bus drains at head-of-cycle, ahead of
the slot steps, so the uplink (a normal async system whose `run` filled its `"commands"` channel out
of band) gets its command dispatched without the one-cycle latency a *tail* cyclic system would
incur. The latency therefore matches WP11's inbox-drain exactly — the difference is purely *where*
the command sits (a registry-tapped message channel vs a coordinator-private `MsgInbox`).

### 4.4 The uplink as a normal async system (Q5, revised)

WP11's uplink was **not a system**: `add_uplink(recv)` stored an `UplinkLauncher` trait object
(`mod.rs:490-502`) with no descriptor, no ports, and no place in the graph; `build()` minted an
`Arc<MsgInbox>` (`mod.rs:1090`); `start()` spawned `run_receiver` (`mod.rs:1697`); and the
coordinator owned a head-of-cycle `drain_uplink` stage (`mod.rs:1714`). All of that is special-case
coordinator plumbing for one feature.

**Decision: the uplink becomes an ordinary `AsyncSystem`** (`src/system/mod.rs:223`) — the read twin
of the `TelemetrySystem` downlink (itself an ordinary `CyclicSystem`, `telemetry/mod.rs:587`).
`UplinkSystem<R: RecvTransport>`:

- **owns its own `RecvTransport`** (its own connection, §4.5) and runs an async `run` loop — the
  current `run_receiver` body (`telemetry/mod.rs:434`): loop `recv`, keep only Msgs whose `id ==
  SequenceCommand::ID`, postcard-decode, drop everything else, `return` on the first error
  (drop-on-disconnect) or stop.
- **re-emits each decoded `SequenceCommand` onto its `"commands"` message channel** via a `MsgOut`
  in its output bundle (the §4.3 emitter capability). Because the wire type and the internal type are
  now identical, this is a **near pass-through**: decode `SequenceCommand` off the socket, `emit` the
  same `SequenceCommand` onto the channel. No mapping, no `MsgInbox`, no translation.

What this **removes** from the coordinator: the `UplinkLauncher`/`UplinkReg` trait + impls
(`mod.rs:487-502`), the `uplink`/`inbox`/`uplink_stop`/`uplink_task`/`uplink_dropped` fields
(`mod.rs:1490-1503`), the `build()` inbox mint (`mod.rs:1090`), the `start()` spawn
(`mod.rs:1694-1700`), and `drain_uplink` (`mod.rs:1714`) — and `MsgInbox`/`run_receiver`
(`telemetry/mod.rs:380-452`) move into the uplink system (or `run_receiver`'s body becomes the
system's `run`; `MsgInbox` is gone, since the command channel *is* the hand-off). `add_uplink`
becomes a thin `add_async` registration (or callers use `add_async`/`add_async_named` directly with
an `UplinkSystem`), so the uplink is wired, validated, sized, and spawned like any async system.

As a normal async system the uplink has **no edge-connected ports** — its only output is the
`"commands"` message channel, which the coordinator drains at head-of-cycle (§4.3). So its
end-to-end latency matches today's inbox-drain: the async task fills the channel out of band, the
coordinator drains it before stepping slots.

### 4.5 Separate connections — shared connection punted (Q7, revised)

WP11 forced the uplink and downlink to **share one TCP connection**: `connect_once(addr)`
(`telemetry/mod.rs:198`) split one `TcpStream` into a write half (downlink `TcpTransport`) + a read
half (uplink `TcpRecvTransport`), and `TcpConn`/`TcpTransport::ensure` stashed an unused `rx`
(`telemetry/mod.rs:103-107,130-134`) as the workaround for the downlink-only path. The motivation
was "no addressing ambiguity at the panel," but it couples two independent systems through one owned
socket and complicates establishment.

**Decision: punt the shared connection. Uplink and downlink each open their own connection** (two
TCP connections to the metor-db broker). The uplink owns a `RecvTransport` that connects on its
own, **subscribes** to the panel's command stream — `MsgStream { msg_id: SequenceCommand::ID }`,
since the db relays a message id only to clients that asked for it (mirroring cube-sat,
`examples/cube-sat/src/main.rs:555`) — and then reads `SequenceCommand`s off a `PacketStream`; the
write half (the `PacketSink` the subscription rode) is held open for the connection's lifetime. The
downlink keeps its lazy-connect `TcpTransport` unchanged. `connect_once` and the unused
`TcpConn.rx`/`TcpTransport` `rx` stash are **removed** — the downlink-only path is the only path
again, and the uplink is just another client of the same endpoint. (Open item: whether the db
needs an `init_world`/handshake before `MsgStream` on a subscribe-only connection — to validate
against a live db.)

**Why shared is deferred, not solved:** a *connection* is an **owned resource that lives outside the
system/ring model**. The framework's whole data-distribution story is the ring — a buffer with no
process-local pointers that the coordinator hands out as cheap `Writer`/`View` *handles*, so two
systems can read/write the same ring trivially. A `TcpStream` is the opposite: a single OS resource
that cannot be cloned into independent handles the way a ring can. Distributing one connection across
two systems (a write half to the downlink, a read half to the uplink) means inventing an ownership
broker the ring model deliberately avoids — who connects, how the split halves reach two tasks, and
how reconnect coordinates across both. That is a deeper systems problem; until the framework grows a
first-class "shared owned resource" abstraction, **two connections is the clean answer**. Shared
uplink+downlink connection is **deferred / future work** (§7).

---

## 5. Sequence messages — the coupling

This is the reason the feature exists. We emit, on the sequence message channel, the two wkt Msgs
the panel's sequence view sources from:

- **`SequenceRegistry`** at boot (and on demand): `channels` = the mission's slots, each
  `SequenceChannelSpec { id, name, available }` where `id` = the **stable per-run slot id** (the
  slot's build-order index, Q2), `name` = the slot instance name, `available` = the slot's
  allowed-occupant names. The coordinator knows all of this at `build()` from the
  `SlotReg`/`AllowedOccupant` set (`src/coordinator/slot.rs:243,278`). The coordinator still
  **builds** the `channel_map` to assign each slot its `channel_id` (for this registry and for the
  `SlotRunner`'s own `channel_id`, `slot.rs:324`) — but after the redesign it no longer uses it to
  *translate* `channel_id → slot name` on the command path (§4.1): dispatch is by `channel_id`
  directly, so the translation accessor (`mod.rs:1548`) is removed even though the build-time
  assignment stays.
- **`SequenceChannelEvent { channel_id, kind }`** on each slot transition.

### 5.1 Host-side emit (locked, Q1)

Emit is **HOST-side, folded into the `SlotRunner`** (`src/coordinator/slot.rs`), with the boot
`SequenceRegistry` emitted by the coordinator. **Zero ABI change**, no occupant change.

The `SlotRunner` already **owns every lifecycle transition** — `do_load`/`do_start`/`do_stop`/
`do_abort`/`do_reset` (`slot.rs:385-454`; `do_unload` is removed with the `Unload` kind, §4.1) — so
it emits each `SequenceChannelEvent`
**at the transition point**, as an every-record message. This is the critical reason not to diff
the latest-wins `SlotStatus` frame: two commands can apply in one cycle's drain (e.g. `Load` then
`Start`), so a `SlotStatus` snapshot would show only `Running` and **lose** the `Loaded` event.
Emitting at the transition captures every transition by construction. (The emit itself is
best-effort: a full events ring drops the event rather than blocking the cycle.)

`Progress` and the terminal **outcome** originate inside the occupant, in its `SequenceStatus`
frame (`run_state` + drained `progress` lines, `src/sequence/mod.rs:261`, `Outcome::run_state` at
`:110`). Today the `SlotRunner` discards this — it maps `FswStatus::Done` → `SlotPhase::Done {
outcome: 0 }` (`slot.rs:499`), losing whether it was Completed/Aborted/Failed. So the host-side
bridge gives the `SlotRunner` a **`View` on its own occupant's `SequenceStatus` output ring**
(that ring is already allocated and registry-tapped, `slot.rs:903`), drains it **every record**
each cycle, and:

- emits `Progress { detail }` for each new progress line, and
- reads `run_state` at terminal to refine `Done` into `Completed` / `Aborted` / `Failed`.

So the `SlotRunner` holds two new things: a `MsgOut` (the sequence message channel) and an
`Input<SequenceStatus>` view on its occupant. Everything else is existing transition code.

### 5.2 The mapping (`SlotPhase` / `SequenceStatus` → wkt)

| Slot-side event | wkt `SequenceEventKind` / state | Source |
|---|---|---|
| `do_load(name)` ok | `Loaded { name }` | transition (`slot.rs:385`) |
| `do_start` | `Started` | transition (`:404`) |
| `do_stop` (hard-drop) | `Stopped` | transition (`:413`) |
| `do_reset(name)` | `Loaded { name }` (re-arm to idle) | transition (`:435`) |
| ~~`do_unload`~~ | ~~`Unloaded`~~ — **removed** with the `Unload` kind (§4.1); the `SequenceEventKind::Unloaded` emit goes unreachable from the command plane | — |
| occupant progress line | `Progress { detail }` | `SequenceStatus.progress` drain |
| `Done`, `run_state=1` | `Completed` | `SequenceStatus.run_state` (`:110`) |
| `Done`, `run_state=2` | `Aborted` | `SequenceStatus.run_state` |
| `Done`, `run_state=3` | `Failed { reason }` | `SequenceStatus.run_state` |
| `Stopped{Panicked}` | `Failed { reason }` (`"panicked"`) | `FswStatus` (`slot.rs`) |

`SequenceRunState` (`../metor-proto/wkt/src/msgs.rs:692`) maps for the registry/channel snapshot:
`Loaded`→`Idle`, `Running`→`Running`, terminal as above. One gap to call out: wkt `Failed {
reason: String }` wants a reason string, but `SequenceStatus` carries only `run_state` (no reason
field). v1 emits a generic reason (`"failed"` / `"panicked"`); a real reason string
needs a field added to `SequenceStatus` (future work).

---

## 6. ABI impact

**None.** The sequence messages are produced entirely host-side by the `SlotRunner`/coordinator
over host-owned heap rings (§5); the command plane is host-side too — the uplink is an ordinary
host-side `AsyncSystem` (§4.4) and the command bus is a host-side message-channel drain (§4.3);
the occupant, `fsw_*`, and `FSW_ABI_VERSION` are untouched. The redesign is wholly FSW-side: it
deletes the `SlotCommand` Frame, the `channel_map` translation, and the `UplinkLauncher`/`MsgInbox`
plumbing, and adds `MsgIn` + a `"commands"` channel — no ABI, no wire, no occupant change.

*If occupant-side emit were ever chosen (deferred):* a message ring is just bytes, so it needs
**no new ABI symbol** — it appears as one more `FswRing` (a byte ring, role OUTPUT) in the
occupant's output array, bound by the existing `RawBinder` (`src/abi/mod.rs:359`) as an
ordinary `MsgOut` (rings are backing-erased — the occupant's port type is the host's). The
`#[sequence]` descriptor would gain one output port. §5.1 makes this
unnecessary for the stated feature.

---

## 7. Scope cut for v1 & future work

**Scope:** the general `MsgOut` emit port + `(PacketId, postcard)` record format; the parallel
`MessageRegistry`; the in-FSW subscriber `MsgIn<M>` (§1.4, **now built** by the command-plane
redesign); telemetry taps **every** message ring and downlinks each record via a non-coalescing
FIFO (no announce); the host-side sequence bridge (`SequenceRegistry` at boot +
`SequenceChannelEvent` per transition, reading the occupant `SequenceStatus`); and the command
plane (§4): one `SequenceCommand` type end-to-end on a `"commands"` message channel, a generic
per-emitter command bus the coordinator drains by `channel_id` at head-of-cycle, and the uplink as
an ordinary `AsyncSystem` over its own connection.

**Done since WP11:** the general in-FSW message **subscriber** `MsgIn<M>` (was deferred; built as
the command bus, §1.4).

**Deferred / future work:**

- **Shared uplink+downlink connection** (§4.5): one socket distributed across two systems needs a
  "shared owned resource" abstraction the ring model doesn't yet have; v1 uses two connections.
- **Direct (non-broadcast) command routing by `channel_id`** (§4.3): the coordinator broadcasts each
  command to every `CyclicSlot::command` and the addressed `SlotRunner` filters; a direct
  `channel_id → slot` dispatch (e.g. an index) would skip the broadcast.
- **KDL-declared command channels** (§2.3): the `"commands"` channel is coordinator-wired in v1; a
  user-declared command emitter via KDL is the same deferred surface as KDL-declared message channels.
- **metor-panel / cube-sat two-connection wiring** (§4.5): this refactor is **FSW-side only** — the
  panel and the cube-sat example must be updated to open a second connection for the uplink (today
  they share one); that wiring is out of scope here.
- KDL-declared per-system message channels (§2.3); ~~**occupant-side** (dl/ABI) message emit
  (§6)~~ — landed with the port unification's schema-tagged `PortDescMsg` (a `.so` declares
  `MsgOut`/`MsgIn` like any port; `docs/design-port-unification.md` §7); a
  `reason` field on `SequenceStatus` so `Failed { reason }` carries detail (§5.2); a
  guaranteed-lossless (backpressured) message ring (§2.1); transport **reconnect/backoff** for both
  directions (v1 is drop-on-disconnect, telemetry.md §7).

---

## 8. Resolved decisions

Every fork is resolved; each entry states the **decision** and keeps the trade-off prose. Entries
1–6 are the WP11 decisions (entry 5 — the uplink form — is **revised** by entry 11). Entry 7 (old
Q7, the shared-connection establishment) is **superseded** by entry 12. Entries 8–12 are the
command-plane redesign (§4).

1. **Sequence emit — DECIDED: HOST-side, in the `SlotRunner` + coordinator boot registry.** The
   `SlotRunner` owns every transition (`slot.rs:385-454`), so events are lossless at the source,
   and it reads the occupant's `SequenceStatus` for `Progress` + terminal outcome — needing
   **zero** ABI/occupant/macro change. *Trade-off:* it couples the otherwise-generic `SlotRunner`
   to wkt sequence semantics (a `MsgOut` + a `SequenceStatus` view + a mapping table land in
   `slot.rs`); occupant-side would keep `SlotRunner` generic but duplicate host-known state, make
   the occupant learn its `channel_id`, and grow the ABI/descriptor. (See §5.)

2. **`channel_id` identity — DECIDED: the slot's build-order index, frozen at `build()`.** Stable
   across the run, trivial to assign, matches `ChannelId` (`u64`). After the redesign it is the
   **direct dispatch key**: `SlotRunner` filters commands by `cmd.channel_id == self.channel_id`
   (§4.1) — the `channel_map` is still built to *assign* each slot its id but no longer used to
   *translate* id → name (§5). *Trade-off:* not stable across wiring edits (insert a slot and ids
   shift); a name-hash would survive reconfiguration but is collision-prone and less legible.
   The panel state is per-session, so the index is fine. (See §5 / §4.1.)

3. **Registry shape — DECIDED: a separate `MessageRegistry`, not a kind-flagged `OutputRegistry`.**
   Half of `RegistryEntry` (`vtable`/`metadata`/`announce`) is dead for self-describing messages,
   and the telemetry announce path is output-only. *Trade-off:* a second parallel structure + a
   second tap loop in telemetry vs one unified registry with dead fields and `if kind == Message`
   branches threaded through output-shaped code. (See §2.2.)

4. **Generality — DECIDED: general `MsgOut` port + `MessageRegistry` + telemetry-taps-all, with
   the sequence bridge wired on top.** The general emit/registry/tap machinery ships in v1 (so any
   host system can emit subscribable messages); KDL-declared per-system channels and occupant-side
   emit are deferred (the in-FSW subscriber `MsgIn<M>` is **now built** — the command bus, §1.4,
   entry 10). *Trade-off:* building the general port now is
   cheap (it is the primitive the bridge needs anyway) and avoids a later refactor, while deferring
   the KDL/subscriber/ABI surface keeps v1 from carrying wiring for consumers that do not yet
   exist. (See §1.2 / §2.3.)

5. **Uplink — DECIDED (WP11; REVISED by entry 11): IN SCOPE, as a head-of-cycle `UplinkSystem`.**
   *WP11 verdict:* the uplink owned a transport read path + an async reader task feeding an
   `Arc<MsgInbox>` that the coordinator drained at a head-of-cycle stage and mapped to `SlotCommand`s
   on the existing command ring. *This was found wrong* — the "uplink" was not a system (no
   descriptor, no ports, not in the graph) and the coordinator was sequence-aware. **Entry 11
   supersedes it:** the uplink is an ordinary `AsyncSystem` re-emitting `SequenceCommand`s onto a
   generic command bus, and the coordinator is sequence-agnostic. The same-cycle property is kept
   (the bus drains at head-of-cycle). (See §4.4.)

6. **Message-ring loss policy — DECIDED (SUPERSEDED by the backpressure refactor, see banner: the
   ring is now lossless-only; a full ring backpressures the emitter and a committed record is never
   lost): generous depth (~64) + every-cycle drain + a surfaced
   `dropped` counter, over a non-coalescing FIFO hand-off.** A large enough burst can still lap;
   loss is surfaced, never silent, and never reordered/latest-wins-dropped like a component
   snapshot. *Trade-off:* truly lossless would need producer backpressure (unacceptable in the
   synchronous cyclic loop) or an unbounded ring (unbounded memory); best-effort-with-surfaced-loss
   matches the framework's "loss, never delay" stance (telemetry.md §4) but means an event log is
   best-effort, not a guarantee. (See §2.1 / §3.1.)

### Superseded: Q7 — uplink connection establishment

7. **DECIDED (WP11; SUPERSEDED by entry 12): one shared bidirectional connection, split.** WP11
   connected once and split one socket into a write half (downlink) + a read half (uplink) via
   `connect_once` (`telemetry/mod.rs:198`), to avoid two inbound sockets at the panel. **Entry 12
   supersedes it:** sharing one owned socket across two systems is a deeper systems problem (a
   connection is an owned resource the ring model can't distribute), so it is punted — uplink and
   downlink use separate connections. (See §4.5.)

### The command-plane redesign (§4)

8. **Command type — DECIDED: collapse to one `SequenceCommand` end-to-end.** WP11's parallel
   `SlotCommand`/`SlotCommandKind` zerocopy Frame (addressed by slot name) and the `channel_map`
   translation are deleted; the internal command plane carries the *wire* `SequenceCommand`
   (addressed by `channel_id`), and `CyclicSlot::command` takes `&SequenceCommand` (`SlotRunner`
   filters by `channel_id`). *Trade-off:* a single Rust type cannot literally ride a Frame ring —
   `SequenceCommand` is postcard/`serde`/variable-length while a Frame ring is `zerocopy`/fixed —
   so the collapse also moves the plane **off the Frame ring onto a postcard message channel**
   (which the downlink already built). The cost is the command plane is now best-effort message
   loss (entry 6) rather than a fixed Frame; the gain is one type, no translation, no `channel_map`
   on the command path, and the coordinator stops being sequence-aware. (See §4.1.)

9. **`Unload` — DECIDED: DROP it.** `SlotCommandKind::Unload` / `SlotRunner::do_unload` are dead
   today (no `SequenceCommand` maps to `Unload`; the wkt kind has no `Unload` variant), and the
   collapse (entry 8) makes them unreachable from any command source. Remove `do_unload` and the
   dead kind; `SequenceCommandKind` is **unchanged** (no wire/ABI change), and the
   `SequenceEventKind::Unloaded` emit goes unreachable on the command path. *Trade-off:* loses
   in-proc `Unload` parity for a host driving slots directly — not wanted (the panel can't express
   it, `Reset` covers re-arm); if ever needed it returns as a host-only coordinator method, not a
   command-plane kind. (See §4.1.)

10. **Command bus shape — DECIDED: per-emitter command channels via the `MessageRegistry`, not one
    shared bus.** Any command emitter declares a `MsgOut` on a reserved `"commands"` channel; the
    coordinator collects every `"commands"` `MessageEntry` and `MsgIn`-drains+dispatches them per
    cycle (the message twin of telemetry's read-all output tap), plus one coordinator-owned channel
    for the in-proc `control_handle()`. The bind-time emitter capability mirrors
    `output_registry()`/`message_registry()` on `RingSource`/`Binder` (`binder.rs:103-138`).
    *Trade-off:* per-emitter is strictly more general than a single shared ring — multiple emitters,
    no writer contention, reuses WP11 infra and the single-writer discipline — at the cost of the
    coordinator iterating the registry for `"commands"` channels each build rather than holding one
    fixed ring. (See §4.3.)

11. **Uplink form — DECIDED: an ordinary `AsyncSystem` (read twin of the downlink).**
    `UplinkSystem<R: RecvTransport>` owns its own `RecvTransport`, runs the `run_receiver` recv loop
    as its `run`, and re-emits each decoded `SequenceCommand` onto its `"commands"` channel — a near
    pass-through now that wire and internal type are identical. `add_uplink` becomes a thin
    `add_async` registration (or callers use `add_async` directly). This **removes** the
    `UplinkLauncher`/`UplinkReg`, the coordinator `uplink`/`inbox`/`drain_uplink` plumbing, and
    `MsgInbox` (the channel is the hand-off). *Trade-off:* revises entry 5 — the uplink is now a
    real graph system (uniform, validated, sized, spawned like any async system) instead of a
    coordinator appendage; same-cycle latency is preserved because the bus drains at head-of-cycle.
    (See §4.4.)

12. **Connection — DECIDED: separate connections; shared is punted.** Uplink and downlink each open
    their own connection; `connect_once` and the unused `TcpConn.rx`/`TcpTransport` `rx` stash are
    removed. *Trade-off:* two TCP connections to the ground station (the panel/cube-sat must be
    updated to open a second, deferred §7) vs sharing one owned socket — which the system/ring model
    cannot cleanly distribute (a connection is an owned OS resource, not a ring handle), so sharing
    needs a "shared owned resource" abstraction that does not exist yet. Two connections is the clean
    answer until it does. (See §4.5.)
