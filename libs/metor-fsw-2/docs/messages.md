# Messages (`messages`)

> **Status: v1 IMPLEMENTED.** Built across WP11 (W1 message channel → W2 downlink → W4
> sequence coupling → W3 uplink → W5 example e2e; plan `messages-plan.md`). The body and the
> resolved decisions (§8) describe the shipped design. One deviation worth noting: the uplink is
> registered via a sibling `CoordinatorBuilder::add_uplink(recv)` rather than a `TelemetryConfig`
> field — it is a coordinator control-plane concern (it owns the command ring, the `MsgInbox`, and
> the `run_for` head stage), and `connect_once(addr) -> (TcpTransport, TcpRecvTransport)`
> preserves the one-shared-bidirectional-socket invariant by splitting a single connect. Verified
> on the real adcs mission: `SequenceRegistry` + the commissioning `SequenceChannelEvent`s
> (Loaded/Started/Progress/Completed, with the real progress strings) are on the downlink, and a
> panel `SequenceCommand` lands the addressed `SlotCommand` the same cycle.
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
> a symmetric **uplink** system that ingests panel→FSW `SequenceCommand` Msgs into the slot
> control plane, and uses both to make the panel's sequence view fully interactive. Every fork is
> resolved (§8), including the uplink's connection establishment (Q7: one shared bidirectional
> socket, established once and split — the plan owns the broker shape).

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
/// per-frame `Output<F>` (src/port.rs). Backing-generic so a (future) dl occupant binds
/// the same port over `RawBacking` (§6).
pub struct MsgOut<B = BoxBacking, WD = NoWake, WS = NoWake> {
    writer: Writer<B, WD, WS>,
    scratch: Vec<u8>,             // reused encode buffer (no per-emit malloc)
}

impl<B: Backing, WD: WakeSource, WS: WakeSink> MsgOut<B, WD, WS> {
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

**v1 has exactly one in-FSW consumer: telemetry** (§3). The general "subscribe" capability —
an in-FSW system *reading* another system's message ring (a typed `MsgIn<M>`) — is deferred. On
the wire, the panel is the real subscriber: it matches incoming `OwnedPacket::Msg` by `id` (the
"catch-all pub/sub" — unmatched wkt Msgs are recorded as telemetry, which is precisely how the
sequence view is fed). So "messages that can be subscribed to" is satisfied by *telemetry
subscribing to all of them* and the wire being a pub/sub bus. The symmetric **ingest** of
panel-published Msgs (commands) back into the FSW is the uplink (§4).

---

## 2. Message rings + registration

### 2.1 Allocation & sizing

A message ring is an ordinary `RingBuffer<BoxBacking>` with `Overrun::Overwrite`, allocated by
the coordinator at `build()` alongside the output rings (`src/coordinator/mod.rs:842-867`). The
only new wrinkle is **sizing**: records are variable, so there is no `capacity_for(F::MAX_SIZE,
depth)`. The heuristic:

```rust
// capacity = next_pow2( frame_len(max_msg_bytes) * depth )
//   max_msg_bytes : per-channel config, default 4 KiB (covers a SequenceRegistry
//                   with a realistic channel count; truncation is a config error)
//   depth         : default 64 — generous, because a message ring is an EVENT/COMMAND
//                   LOG, not a latest-wins snapshot; bursts of events between two
//                   telemetry drains must not lap.
```

Because the ring is `Overwrite` and bounded, a sufficiently large burst *can* still lap an
un-drained reader; telemetry drains **every cycle** and surfaces any lap as a `dropped` counter
(§3, Q6). Generous depth + per-cycle drain is the mitigation; a guaranteed-lossless message ring
is out of scope (it would need backpressure into the producer, which the cyclic loop cannot
afford).

### 2.2 The `MessageRegistry`

A parallel index to `OutputRegistry` (`src/registry.rs`), because the registry's per-entry
fields (`vtable`, `metadata`, prefixed `announce`) are meaningless for a self-describing message
ring (Q3):

```rust
pub struct MessageEntry {
    pub instance: Arc<str>,                       // owning system instance (subset filtering)
    pub channel: Arc<str>,                        // the message-channel name (telemetry id)
    pub(crate) ring: RingBuffer<BoxBacking>,      // view()-only, slot-accounted (registry.rs:47)
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
deferred, so the only channel *wired* in v1 is the host-side sequence bridge's (§5), allocated by
the coordinator directly.

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

In-cycle `execute` (`:437`) drains **every** record from each message view (loop `try_read_into`
until `Ok(false)`, like `Input::drain`, `src/port.rs:229` — not the latest-wins `loop` the output
taps use at `:446`), splits the 2-byte id, builds `LenPacket::msg(id, payload.len())`
(`../metor-proto/src/types.rs:652`), extends it with the payload, and pushes each to the FIFO in
ring order. A lapped view bumps `telemetry.dropped` and resyncs.

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

## 4. The uplink system (panel → FSW)

The downlink makes the panel's sequence view *visible*; the uplink makes it *interactive*. The
panel publishes `SequenceCommand { channel_id, command }` Msgs (`msgs.rs:753`) to load/start/
stop/abort channels. **`UplinkSystem` is the read twin of `TelemetrySystem`**: it owns a
transport read path and an async reader task, and it lands each command into the slot control
plane the **same cycle** it arrives.

### 4.1 Form — a head-of-cycle coordinator stage (Q5)

The constraint "a command lands the same cycle" pins the form. The slots feature already drains
the coordinator's `SlotCommand` control ring **at the top of the cycle, before the slots step**
(`sequences-slots.md §3`; the command ring + `control_handle()` live at
`src/coordinator/mod.rs:931,1284`). For a panel command to dispatch the same cycle, the uplink
must write its `SlotCommand` into that ring **before** that drain.

A plain `CyclicSystem` registered first does **not** achieve this: cyclic `execute`s run *after*
the command drain (slots are themselves `CyclicSlot`s stepped in that phase), so a command a
first-registered cyclic system wrote would not be drained until the next cycle — a one-cycle
latency. So the uplink's in-cycle stage is **folded into the coordinator head**, immediately
ahead of the existing `SlotCommand` drain:

```text
   cycle N:
     ├─ uplink.drain_inbox()    ← NEW: map SequenceCommand→SlotCommand, write to command ring
     ├─ drain SlotCommand ring   (existing, sequences-slots.md §3) → dispatch to SlotRunners
     ├─ step slots / cyclic systems
     └─ telemetry (downlink) snapshot          (registered last)
```

Structurally it is still "a system like the downlink" — it owns a `Transport` + an async task +
a bounded in-cycle hand-off — but it is wired as the cycle's **head** stage rather than a tail
`CyclicSystem`, because tail placement costs a cycle. The async/in-cycle split mirrors telemetry
exactly, inverted:

```rust
/// The inbound twin of HandOff: the async reader task fills it, the head-of-cycle
/// stage drains it. Bounded FIFO (commands are rare; overflow drops oldest + counts).
struct MsgInbox {
    queue: Mutex<VecDeque<SequenceCommand>>,
    dropped: AtomicU64,
    /* pending, wq — the cycle polls it each tick, never parks on it */
}

/// The async reader task: own the connection's READ half, loop recv, keep only
/// SequenceCommand Msgs (cube-sat's exact filter, examples/cube-sat/src/main.rs:690).
async fn run_receiver<R: RecvTransport>(mut rx: R, inbox: Arc<MsgInbox>, stop: Arc<AtomicBool>) {
    while !stop.load(Acquire) {
        match rx.recv().await {
            Ok(OwnedPacket::Msg(m)) if m.id == SequenceCommand::ID => {
                if let Ok(cmd) = m.parse::<SequenceCommand>() { inbox.push(cmd); }
            }
            Ok(_) => {}                       // other Msgs/Tables ignored (uplink is commands only)
            Err(_) => return,                  // drop-on-disconnect, like the sender (telemetry.md §7)
        }
    }
}
```

### 4.2 The transport read path

The v1 `Transport` is **write-only** (`announce`/`send`, `src/telemetry/mod.rs:59-71`). The
uplink adds a read surface — a `recv` that yields the next inbound `OwnedPacket`:

```rust
/// The read twin of `Transport`. v1: a `PacketStream` over the connection's read half,
/// the inverse of `PacketSink` (the sender already holds the read half unused — TcpConn.rx,
/// telemetry/mod.rs:76). Mirrors cube-sat's `PacketStream`/`spawn_recv` (main.rs:545-546).
pub trait RecvTransport {
    async fn recv(&mut self) -> Result<OwnedPacket<...>, TransportError>;
}
```

**Socket: one shared bidirectional connection (recommended).** The FSW connects *out* to the
panel/db and the panel replies on that **same** socket — exactly what cube-sat does on one
connection (`examples/cube-sat/src/main.rs:541-543` splits one `TcpStream::connect` into
`rx`/`tx`; `tx.send`s telemetry and the same connection's `rx` ingests `SequenceCommand` at
`:690`). `TcpTransport` already holds both halves (`TcpConn { sink, rx }`, `telemetry/mod.rs:75`)
and keeps `rx` alive but unused — so the read half is *already there*. The downlink sender task
drives the write half (`sink`); the uplink reader task drives the read half (`rx`). One socket
means the panel has **no addressing ambiguity** (it answers on the connection it accepted); two
independent connections would make the panel field two inbound sockets from one FSW and guess
which carries the uplink.

The wrinkle: `TcpTransport` connects **lazily inside the sender task** on first announce
(`ensure`, `:97`). To hand the read half to a *separate* reader task, connection establishment
must move to a single shared point that splits the stream and gives each task its half. The
**precise establishment mechanism** (who connects, how the split halves reach two tasks, and
reconnect behavior across both) is the one new open question the uplink raises — see Q7. The
recommendation (shared connection) is settled; only its plumbing needs a human nod.

### 4.3 Mapping `SequenceCommand` → `SlotCommand`

The coordinator holds the **channel↔slot map** built when it emits the boot `SequenceRegistry`
(§5): `channel_id` (the slot's build-order index) ↔ slot instance name. The head-of-cycle stage
uses it to translate each inbound command and write it to the existing command ring (reusing the
slots feature's dispatch entirely):

| wkt `SequenceCommandKind` (`msgs.rs:738`) | `SlotCommand` (`slot.rs:109,163`) |
|---|---|
| `Load { name }` | `SlotCommand::load(slot, name)` |
| `Start` | `SlotCommand::start(slot)` |
| `Abort` | `SlotCommand::abort(slot)` |
| `Stop` | `SlotCommand::stop(slot)` |
| `Reset` | `SlotCommand::reset(slot)` |

`slot` = `channel_map[channel_id]`; an unknown `channel_id` is dropped (logged). The occupant
name on `Load` is the wkt `name` verbatim (it must be in the slot's allowed set, which the
`SlotRunner` already validates, `slot.rs:392`). The downlink (`MsgOut`/§3) is the emit side; the
uplink is the symmetric ingest side — together they make load/start/stop/abort from metor-panel
drive real slots, and the resulting transitions flow back out as `SequenceChannelEvent`s (§5).

---

## 5. Sequence messages — the coupling

This is the reason the feature exists. We emit, on the sequence message channel, the two wkt Msgs
the panel's sequence view sources from:

- **`SequenceRegistry`** at boot (and on demand): `channels` = the mission's slots, each
  `SequenceChannelSpec { id, name, available }` where `id` = the **stable per-run slot id** (the
  slot's build-order index, Q2), `name` = the slot instance name, `available` = the slot's
  allowed-occupant names. The coordinator knows all of this at `build()` from the
  `SlotReg`/`AllowedOccupant` set (`src/coordinator/slot.rs:243,278`), and it **keeps the
  channel↔slot map** the uplink reuses (§4.3).
- **`SequenceChannelEvent { channel_id, kind }`** on each slot transition.

### 5.1 Host-side emit (locked, Q1)

Emit is **HOST-side, folded into the `SlotRunner`** (`src/coordinator/slot.rs`), with the boot
`SequenceRegistry` emitted by the coordinator. **Zero ABI change**, no occupant change.

The `SlotRunner` already **owns every lifecycle transition** — `do_load`/`do_start`/`do_stop`/
`do_abort`/`do_reset`/`do_unload` (`slot.rs:385-454`) — so it emits each `SequenceChannelEvent`
**at the transition point**, as an every-record message. This is the critical reason not to diff
the latest-wins `SlotStatus` frame: two commands can apply in one cycle's drain (e.g. `Load` then
`Start`), so a `SlotStatus` snapshot would show only `Running` and **lose** the `Loaded` event.
Emitting at the transition is lossless by construction.

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
| `do_unload` | `Unloaded` | transition (`:449`) |
| occupant progress line | `Progress { detail }` | `SequenceStatus.progress` drain |
| `Done`, `run_state=1` | `Completed` | `SequenceStatus.run_state` (`:110`) |
| `Done`, `run_state=2` | `Aborted` | `SequenceStatus.run_state` |
| `Done`, `run_state=3` | `Failed { reason }` | `SequenceStatus.run_state` |
| `Stopped{Lapped/Panicked}` | `Failed { reason }` (`"lapped"`/`"panicked"`) | `FswStatus` (`slot.rs:500-505`) |

`SequenceRunState` (`../metor-proto/wkt/src/msgs.rs:692`) maps for the registry/channel snapshot:
`Loaded`→`Idle`, `Running`→`Running`, terminal as above. One gap to call out: wkt `Failed {
reason: String }` wants a reason string, but `SequenceStatus` carries only `run_state` (no reason
field). v1 emits a generic reason (`"failed"` / `"lapped"` / `"panicked"`); a real reason string
needs a field added to `SequenceStatus` (future work).

---

## 6. ABI impact

**None.** The sequence messages are produced entirely host-side by the `SlotRunner`/coordinator
over `BoxBacking` rings (§5); the uplink ingest is host-side too (§4); the occupant, `fsw_*`, and
`FSW_ABI_VERSION` are untouched. This is the decisive payoff of host-side emit + a host-side
uplink stage.

*If occupant-side emit were ever chosen (deferred):* a message ring is just bytes, so it needs
**no new ABI symbol** — it appears as one more `FswRing` (a byte ring, role OUTPUT) in the
occupant's output array, bound by the existing `RawBinder` (`src/abi/mod.rs:359`) as a
`MsgOut<RawBacking>`. The `#[sequence]` descriptor would gain one output port. §5.1 makes this
unnecessary for the stated feature.

---

## 7. Scope cut for v1 & future work

**v1 scope:** the general `MsgOut` emit port + `(PacketId, postcard)` record format; the parallel
`MessageRegistry`; telemetry taps **every** message ring and downlinks each record via a
non-coalescing FIFO (no announce); the host-side sequence bridge (`SequenceRegistry` at boot +
`SequenceChannelEvent` per transition, reading the occupant `SequenceStatus`); and the symmetric
**`UplinkSystem`** ingesting panel `SequenceCommand` Msgs into the slot command ring at the head
of the cycle, over a shared bidirectional connection.

**Deferred / future work:** KDL-declared per-system message channels (§2.3); a general in-FSW
message **subscriber** (`MsgIn<M>`, §1.3); **occupant-side** (dl/ABI) message emit (§6); a
`reason` field on `SequenceStatus` so `Failed { reason }` carries detail (§5.2); a
guaranteed-lossless (backpressured) message ring (§2.1); transport **reconnect/backoff** for both
directions (v1 is drop-on-disconnect, telemetry.md §7).

---

## 8. Resolved decisions

Every fork is resolved; each entry states the **decision** and keeps the trade-off prose. One
**new** open question (Q7) the uplink raised needs a human nod before planning.

1. **Sequence emit — DECIDED: HOST-side, in the `SlotRunner` + coordinator boot registry.** The
   `SlotRunner` owns every transition (`slot.rs:385-454`), so events are lossless at the source,
   and it reads the occupant's `SequenceStatus` for `Progress` + terminal outcome — needing
   **zero** ABI/occupant/macro change. *Trade-off:* it couples the otherwise-generic `SlotRunner`
   to wkt sequence semantics (a `MsgOut` + a `SequenceStatus` view + a mapping table land in
   `slot.rs`); occupant-side would keep `SlotRunner` generic but duplicate host-known state, make
   the occupant learn its `channel_id`, and grow the ABI/descriptor. (See §5.)

2. **`channel_id` identity — DECIDED: the slot's build-order index, frozen at `build()`.** Stable
   across the run, trivial to assign, matches `ChannelId(u64)`, and doubles as the uplink's
   channel↔slot map (§4.3). *Trade-off:* not stable across wiring edits (insert a slot and ids
   shift); a name-hash would survive reconfiguration but is collision-prone and less legible.
   v1's panel state is per-session, so the index is fine. (See §5 / §4.3.)

3. **Registry shape — DECIDED: a separate `MessageRegistry`, not a kind-flagged `OutputRegistry`.**
   Half of `RegistryEntry` (`vtable`/`metadata`/`announce`) is dead for self-describing messages,
   and the telemetry announce path is output-only. *Trade-off:* a second parallel structure + a
   second tap loop in telemetry vs one unified registry with dead fields and `if kind == Message`
   branches threaded through output-shaped code. (See §2.2.)

4. **Generality — DECIDED: general `MsgOut` port + `MessageRegistry` + telemetry-taps-all, with
   the sequence bridge wired on top.** The general emit/registry/tap machinery ships in v1 (so any
   host system can emit subscribable messages); KDL-declared per-system channels, an in-FSW
   subscriber, and occupant-side emit are deferred. *Trade-off:* building the general port now is
   cheap (it is the primitive the bridge needs anyway) and avoids a later refactor, while deferring
   the KDL/subscriber/ABI surface keeps v1 from carrying wiring for consumers that do not yet
   exist. (See §1.2 / §2.3.)

5. **Uplink — DECIDED: IN SCOPE, as a head-of-cycle `UplinkSystem` (the read twin of the
   downlink).** It owns a transport read path + an async reader task (mirroring `TelemetrySystem`
   inverted), runs as the cycle's **head** stage so a command lands the **same cycle**, and writes
   `SlotCommand`s into the existing command ring to reuse the slots feature's dispatch
   (`mod.rs:931,1284`). *Trade-off:* a plain `CyclicSystem` registered first would be more uniform
   but costs a one-cycle latency (cyclic `execute`s run *after* the command drain), so the uplink
   is folded into the coordinator head instead; and it pulls a bidirectional transport into an
   otherwise downlink-shaped feature — but that is the cost of closing the operator loop, and the
   `SequenceCommandKind` ↔ `SlotCommandKind` mapping is mechanical (§4.3). (See §4.)

6. **Message-ring loss policy — DECIDED: generous depth (~64) + every-cycle drain + a surfaced
   `dropped` counter, over a non-coalescing FIFO hand-off.** A large enough burst can still lap;
   loss is surfaced, never silent, and never reordered/latest-wins-dropped like a component
   snapshot. *Trade-off:* truly lossless would need producer backpressure (unacceptable in the
   synchronous cyclic loop) or an unbounded ring (unbounded memory); best-effort-with-surfaced-loss
   matches the framework's "loss, never delay" stance (telemetry.md §4) but means an event log is
   best-effort, not a guarantee. (See §2.1 / §3.1.)

### Resolved: Q7 — uplink connection establishment

7. **DECIDED: one shared bidirectional connection, established once and split.** The FSW connects
   out, the panel replies on the same socket (cube-sat's pattern, `main.rs:541-543,690`; `TcpConn`
   already holds the unused read half, `telemetry/mod.rs:76`). Two independent connections were
   rejected: the panel would field two inbound sockets from one FSW with no clean way to tell which
   carries the uplink. The **establishment mechanism** is approach (a): hoist connect from the
   lazy `ensure` (`:97`) to a single shared point that connects once, splits the stream, and hands
   the write half to the downlink sender and the read half to the uplink reader (a small
   `connect() -> (Tx, Rx)` both tasks take). *Implementation constraint for the plan:* this must not
   regress the existing downlink — the lazy-connect restructure should preserve the
   connect-once / no-reconnect / silent-drop-on-failure behavior (telemetry.md §7), now shared by
   both halves (a dead socket takes both down together, which is the intended v1 semantics).
   Reconnect remains future work. The plan owns the precise broker shape (who establishes, how the
   two halves reach the sender and the head-of-cycle uplink stage).
