# Implementation plan — messages (downlink + uplink + sequence coupling)

> **Since superseded:** messages gained full wiring-parity ports/edges (`docs/message-wiring.md`,
> WP1-WP9), and the WP11 command plane this plan built (a `SlotCommand` Frame ring, a shared
> uplink/downlink socket, `channel_id` dispatch) was reframed twice more — see `docs/messages.md`'s
> status banner for the fully current shape (name-addressed commands, explicit per-slot message
> edges, separate uplink/downlink connections, one unified `Registry`). Kept as the W1-W5
> implementation history.

Design: `docs/messages.md` (approved, decisions locked; Q7 resolved — one shared bidirectional
socket, established once and split, broker shape owned by this plan). Summary of what we build: a
second payload kind — **messages** (`(PacketId, postcard-bytes)` records on byte rings, the
`MsgOut` emit port + a parallel `MessageRegistry`), telemetry **downlinks** every message record
as `OwnedPacket::Msg` over a non-coalescing FIFO (no announce), a symmetric **uplink** ingests
panel `SequenceCommand` Msgs into the slot command ring at the head of the cycle over a shared
bidirectional connection, and the **sequence coupling** emits `SequenceRegistry` + per-transition
`SequenceChannelEvent`s host-side so the panel's sequence view is fully interactive. **Zero ABI
change** (`docs/messages.md` §6).

Built in **5 waves**. Dependency graph (strict edges →):

```
W1 (message channel) ─┬─▶ W2 (downlink tap) ─▶ W3 (uplink) ─┐
                      └─▶ W4 (sequence coupling) ───────────┴─▶ W5 (example + e2e)
```

- **W2 and W4 are independent** after W1 — different files (`src/telemetry/` vs
  `src/coordinator/slot.rs`) — and can run in parallel.
- **W3 depends on W2** (it restructures the same `TcpTransport`/`run_sender` connection path; do
  not have two waves rewriting the transport concurrently), and on W1's record format.
- **W4 depends on W1** (the `MsgOut` port) and on the existing `SlotRunner` already in the tree
  (`src/coordinator/slot.rs`).
- **W5 depends on W2 + W3 + W4** (it asserts `SequenceRegistry`/events on the wire *and* injects a
  `SequenceCommand` through the uplink).
- **Cross-wave coupling to watch:** the **channel↔slot map** (slot-build-order-index → slot name,
  Q2) is introduced in **W3** (the uplink needs it to address commands) and **reused** in **W4**
  (the boot `SequenceRegistry` payload). Build it once, in the coordinator, in W3; W4 only reads
  it. See Risk R4.

**Critical path: W1 → W2 → W3 → W5.** W4 joins before W5 (parallel to W2/W3 after W1). Build +
test after each wave; `cargo build -p metor-fsw-2 --no-default-features` must stay green at every
wave — **the entire message/downlink/uplink/coupling surface is ungated** (none of it is
`kdl`-only; it lives beside `telemetry`/`coordinator`/`registry`, all ungated, and uses
`metor_proto`/`metor_proto_wkt`, both already non-optional deps, `Cargo.toml:12,15`).

---

## Wave 1 — the general message channel (`src/message.rs` new, `src/registry.rs`, `src/coordinator/mod.rs`)

**Independent (only W1).** The `(PacketId, postcard)` record format, the `MsgOut` emit port, the
parallel `MessageRegistry`, and coordinator-side message-ring allocation/registration — the
primitive every later wave builds on (`docs/messages.md` §1, §2). Ungated.

> **Confirm first (it's already in the tree):** `Msg` (`trait Msg: Serialize { const ID: PacketId
> }`, `../metor-proto/src/types.rs:590`), `PacketId = [u8; 2]` (`:554`), `LenPacket::msg(id, cap)`
> (`:652`) + `LenPacket::extend_from_slice` (`:714`), `OwnedPacket::Msg(MsgBuf { id, bytes })`
> (`:761,853`); the ring already takes arbitrary byte records via `Writer::try_write(&[u8])`
> (`ring/src/lib.rs:859`). Nothing in metor-proto needs to change.

1. **`src/message.rs`** (new module; `mod message;` in `src/lib.rs` beside `mod port;` `:19`,
   re-export from the root beside `pub use port::{…}` `:70`):
   - **`MsgOut<B = BoxBacking, WD = NoWake, WS = NoWake>`** — type-erased over the Msg type
     (`docs/messages.md` §1.2). Wraps one `Writer<B, WD, WS>` (the same writer `Output<F>` holds,
     `src/port.rs:55`) plus a reused `scratch: Vec<u8>`. `emit<M: Msg>(&mut self, msg: &M) ->
     Result<(), WriteError>`: `scratch.clear()`, push the 2-byte `M::ID`, postcard-serialize the
     payload into `scratch` (`postcard::serialize_with_flavor`, the same call
     `IntoLenPacket for &M` uses, `../metor-proto/src/types.rs:620-625`), `writer.try_write(&scratch)`.
   - **`MsgOut::new(writer)`** and a **`MsgOut::bind<S: RingSource<B=B>>`** mirroring
     `Output::bind` (`src/port.rs:89`) for the future binder path — but v1 only uses `new` over a
     coordinator-allocated ring (the user-bundle/binder integration is deferred, see scope note).
   - **Record helpers** `pub fn split_record(rec: &[u8]) -> Option<(PacketId, &[u8])>` (the tap's
     inverse, used by W2): first 2 bytes = id, rest = payload.
   - **Sizing** `pub fn msg_capacity(max_msg_bytes: usize, depth: usize) -> usize` =
     `(frame_len(max_msg_bytes) * depth.max(2)).next_power_of_two()` — the `capacity_for`
     (`src/port.rs:33`) analogue for variable records. Defaults `MAX_MSG_BYTES = 4096`,
     `MSG_DEPTH = 64` (generous — an event/command **log**, not a snapshot; `docs/messages.md` §2.1).
2. **`src/registry.rs`** — add the parallel **`MessageRegistry`** + **`MessageEntry`** beside
   `OutputRegistry`/`RegistryEntry` (`src/registry.rs:23,53`), **no** `vtable`/`metadata`/`announce`
   (messages are self-describing, Q3):
   - `MessageEntry { key: ComponentId, instance: Arc<str>, channel: Arc<str>, ring:
     RingBuffer<BoxBacking> }` with `view() -> Result<View<…>, FullReaderTable>` (slot-accounted,
     exactly `RegistryEntry::view`, `:47`).
   - `MessageRegistry { entries, by_key }` with `entries()`, `get(key)`, `view(key)`, `len`,
     `is_empty` — the `OutputRegistry` shape verbatim (`:58-97`).
3. **`src/coordinator/mod.rs`** — allocate + register message rings at `build()`:
   - A **`msg_ring`/`msg_writer` helper** pair mirroring `coord_ring`/`slot_writer`
     (`src/coordinator/slot.rs:547`): `msg_ring(max_bytes, depth, readers) ->
     RingBuffer<BoxBacking>` (overwrite, `msg_capacity` sizing) and `msg_writer(ring) ->
     MsgOut<BoxBacking>`.
   - A **`Vec<MessageEntry>` collected alongside `reg_entries`** in the build pass
     (`src/coordinator/mod.rs:838`), **frozen into `Arc<MessageRegistry>` next to the
     `OutputRegistry` freeze** (`:942`), and **stored on `Coordinator`** beside `registry`
     (`:1238`). v1 allocates **no** message rings yet from generic systems — W4 adds the sequence
     channels; this wave lands the registry plumbing + the freeze so W4/W2 have it. Each message
     ring is sized for `n_registry_consumers` readers (`:840`) like an output (telemetry taps it).
   - Expose **`Coordinator::message_registry(&self) -> Arc<MessageRegistry>`** beside
     `registry()` (`:1273`).

**New public surface:** `crate::message::{MsgOut, msg_capacity, split_record, MAX_MSG_BYTES,
MSG_DEPTH}`; `crate::registry::{MessageRegistry, MessageEntry}` (re-exported beside
`OutputRegistry`, `src/lib.rs:59`); `Coordinator::message_registry`.

**Scope note (deferred, per design Q4):** a *user system* declaring a `MsgOut` inside its
`SystemOutput` bundle and having the **binder/descriptor** size+bind it is **out of scope** —
`PortDesc` is frame-typed (vtable/max_size, `src/descriptor.rs`), so message ports would need a
separate descriptor list. v1 mints `MsgOut`s coordinator-side (the `msg_writer` path, exactly how
the coordinator already mints its own `status_out`/`control` writers, `slot.rs:547`), which is all
W4 needs. The user-bundle path + KDL declaration are future work.

**Verify:**
- `cargo build -p metor-fsw-2` and `cargo build -p metor-fsw-2 --no-default-features`.
- `cargo test -p metor-fsw-2 message::` (new `src/message.rs#[cfg(test)]`): mint a `MsgOut` over a
  fresh `RingBuffer`, `emit(&SequenceRegistry{ … })` (wkt, already a dep), claim a `View` via a
  `MessageEntry`, read the record, assert `split_record` yields `SequenceRegistry::ID` and that
  `postcard::from_bytes::<SequenceRegistry>(payload)` round-trips. A second emit of a different
  Msg type on the same `MsgOut` confirms the type-erasure.

---

## Wave 2 — the downlink tap (`src/telemetry/mod.rs`)

**Depends on W1.** Telemetry taps every `MessageRegistry` ring and downlinks each record as
`OwnedPacket::Msg`, over a **non-coalescing** FIFO distinct from the latest-wins component
`HandOff` (`docs/messages.md` §3). Ungated.

1. **Pull the `MessageRegistry`** in `TelemetryPorts::bind` (`src/telemetry/mod.rs:291-299`): add
   a `messages: Arc<MessageRegistry>` field alongside `registry` (the `RingSource` already hands
   the host `OutputRegistry`; extend the binder’s `output_registry()` analogue, or pull
   `message_registry` from the same host `Binder`). Bump nothing in `add_telemetry`
   (`:581`) — the same single registry-consumer count covers message rings (they are sized for
   `n_registry_consumers`, W1).
2. **`MsgHandOff`** (new, beside `HandOff` `:188-228`): a **bounded FIFO**, not a per-tap
   `Vec<Option<LenPacket>>`:
   ```rust
   struct MsgHandOff {
       queue: Mutex<VecDeque<LenPacket>>,   // bounded cap (e.g. 1024); NOT latest-wins
       dropped: AtomicU64,                  // overflow drops the OLDEST + counts (surfaced)
       pending: AtomicBool,
       wq: WaitQueue,
   }
   ```
   `push(pkt)` appends; if at cap, `pop_front()` first and `dropped += 1`. `drain() -> Vec<LenPacket>`
   takes all in FIFO order (drops the lock before any `.await`, like `HandOff::drain` `:224`). One
   **shared** FIFO across all message taps (cross-channel order is irrelevant — each Msg carries its
   own `channel_id`, `docs/messages.md` §3.2); simpler than per-tap queues. See Risk R2.
3. **Message taps** on `TelemetrySystem` (`:334`): add `msg_taps: Vec<MsgTap>` (a `View` per
   `MessageEntry` whose `mode.matches`-equivalent passes; reuse `TelemetryMode`, but messages have
   no `frame_id` — filter on `instance`/`channel` only) and `msg_handoff: Option<Arc<MsgHandOff>>`.
   In `init` (`:372`), after the output-tap loop, walk `messages.entries()` and claim a `View` each
   (surface a `telemetry.reader_slot` error + skip on `FullReaderTable`, like `:385`). **No
   announce** for message taps (skip the `announces.push` step entirely — messages are
   self-describing, `:398`).
4. **In-cycle drain** in `execute` (`:437`), after the output-tap loop: for each `msg_tap`, drain
   **every** record (loop `try_read_into` until `Ok(false)` — the `Input::drain` shape,
   `src/port.rs:229`, **not** the latest-wins `loop` at `:446`), `split_record` (W1) to `(id,
   payload)`, build `LenPacket::msg(id, payload.len())` + `extend_from_slice(payload)`
   (`../metor-proto/src/types.rs:652,714`), and `msg_handoff.push(pkt)`. A lapped view bumps
   `telemetry.dropped` and `resync`s (`:451`). Surface `msg_handoff.dropped` as
   `telemetry.msg_dropped` health (the `last_dropped` pattern, `:463-467`).
5. **Sender** (`run_sender`, `:240`): also drain the `MsgHandOff` and `transport.send(pkt)` each
   pending message packet — `Transport::send` already takes any `LenPacket` and writes it verbatim
   (`:70,132`), so a `LenPacket::msg(...)` needs **no** new send surface. Wake the sender from
   *either* hand-off (`wq.wake_all`, `:221`); on `stop`, drain/exit as today (`:256`).

**New public surface:** none required beyond W1 (internal to `telemetry`); optionally export
`TelemetryMode` filtering of message channels (already public).

**Verify:**
- `cargo build -p metor-fsw-2` and `--no-default-features`.
- `cargo test -p metor-fsw-2 telemetry::` — extend `src/telemetry/tests.rs` (the `MockTransport`
  records `Vec<LenPacket>`, `:107,143`): build a coordinator (or a hand-built registry) with one
  message ring, emit two Msgs **and** a component frame, run the cycle + sender, and assert the
  `MockTransport`'s captured packets contain **both** Msg packets in order (parse each back via
  `OwnedPacket::parse`/`MsgBuf`, `../metor-proto/src/types.rs:786`) and that they were **not**
  coalesced (emit N events, assert N received). Confirm the existing latest-wins component-snapshot
  tests (`:373` the drop test) still pass — the two hand-offs are independent.

---

## Wave 3 — the uplink (`src/telemetry/` transport, `src/coordinator/mod.rs`)

**Depends on W2** (it restructures the same connection path) **and W1** (record format). The
`RecvTransport` read path, the shared-connection broker, the `run_receiver` task + `MsgInbox`, and
the **head-of-cycle** uplink stage that maps `SequenceCommand → SlotCommand` and writes the
existing command ring (`docs/messages.md` §4). Ungated. **Highest-risk wave — see R1.**

1. **`RecvTransport` trait** (`src/telemetry/mod.rs`, beside `Transport` `:59`):
   `async fn recv(&mut self) -> Result<OwnedPacket<…>, TransportError>` — the read twin of
   `send`. The mock test impl yields injected packets; `TcpTransport`'s impl reads via a
   `PacketStream` over the read half (the inverse of `PacketSink`, mirroring cube-sat's
   `PacketStream`/`spawn_recv`, `examples/cube-sat/src/main.rs:545-546`).
2. **Shared-connection broker (the R1 restructure).** Today `TcpTransport` connects **lazily inside
   the sender task** on first announce (`ensure`, `:97-110`) and holds both halves unused-read
   (`TcpConn { sink, rx }`, `:75`). Restructure so **one** connection is established **once** and
   **split**, the write half to the downlink sender and the read half to the uplink reader:
   - Introduce a small **`Connection` broker**: a `connect_once()` that performs the single
     `TcpStream::connect(addr).split()` (`examples/cube-sat/src/main.rs:541-543`) and yields
     `(PacketSink<…>, PacketStream<…>)`. The downlink sender takes the sink; the uplink reader
     takes the stream.
   - **Preserve the existing behavior exactly** (`telemetry.md §7`): **connect-once, no reconnect,
     silent drop on disconnect**. Both tasks must observe the *same* connection's lifetime — if the
     socket drops, the sender stops downlinking (today's `:268` `return`) **and** the reader stops
     ingesting (its `run_receiver` returns on `recv` error). The lazy-on-first-announce timing may
     move to "established at telemetry init/first-use at a shared point"; that is acceptable, but
     **must not** add reconnection or change the drop-on-error semantics. Keep the `Mock`/test path
     able to supply both halves without a real socket.
3. **`MsgInbox` + `run_receiver`** (`src/telemetry/mod.rs` or a new `src/telemetry/uplink.rs`):
   - `MsgInbox { queue: Mutex<VecDeque<SequenceCommand>>, dropped: AtomicU64 }` — bounded; the
     cycle polls it, never parks (the inbound twin of `MsgHandOff`).
   - `async fn run_receiver<R: RecvTransport>(rx, inbox, stop)`: loop `rx.recv()`, keep only
     `OwnedPacket::Msg(m) if m.id == SequenceCommand::ID`, `m.parse::<SequenceCommand>()`
     (cube-sat's exact filter, `examples/cube-sat/src/main.rs:690-696`), `inbox.push(cmd)`; ignore
     other packets; `return` on error (drop-on-disconnect). Spawn it from telemetry `init`
     alongside `run_sender` (`src/telemetry/mod.rs:408`), guarded by the same `stop` flag.
4. **The channel↔slot map** (`src/coordinator/mod.rs`): build it at `build()` from the registered
   slots — `Vec<(ChannelId, &'static str)>` where `ChannelId` = the slot's **build-order index**
   among slots (Q2), `name` = the slot instance name (the `SlotReg`/`SlotRunner.name`,
   `src/coordinator/slot.rs:278,295`). Store on `Coordinator`. This is just the slot-name list; W4
   reuses it for the `SequenceRegistry` payload (R4).
5. **Head-of-cycle uplink stage** in `run_for` (`src/coordinator/mod.rs:1317`): **before**
   `drain_commands()` (`:1337`), drain the `MsgInbox`, map each `SequenceCommand{channel_id, kind}`
   → `SlotCommand` via the channel↔slot map + the kind table below, and **write it to the existing
   command ring** (the in-proc `control_handle()` writer, `:1284`, or a coordinator-held
   `Output<SlotCommand>` over `command_ring`, `:1243`). The existing `drain_commands()` (`:1411`,
   `command_view.drain` → broadcast to slots) then dispatches it **the same cycle**. An unknown
   `channel_id` is dropped (logged). Mapping (`SequenceCommandKind` `msgs.rs:738` → `SlotCommand`
   `slot.rs:163`):

   | wkt kind | `SlotCommand` |
   |---|---|
   | `Load { name }` | `SlotCommand::load(slot, &name)` |
   | `Start` | `SlotCommand::start(slot)` |
   | `Abort` | `SlotCommand::abort(slot)` |
   | `Stop` | `SlotCommand::stop(slot)` |
   | `Reset` | `SlotCommand::reset(slot)` |

   The uplink reaches the command ring through the **handle the coordinator already exposes** —
   the slots feature's command ring + `drain_commands` dispatch is reused wholesale; no new slot
   dispatch.
6. **Wire it into `add_telemetry`/config** (`src/coordinator/mod.rs:581`): the telemetry config
   gains an optional uplink (the `RecvTransport` half + the `MsgInbox` the coordinator polls). The
   coordinator holds the `MsgInbox` (so its `run_for` head stage can drain it) and the
   broker hands the read half to `run_receiver`. Keep a downlink-only configuration valid (uplink
   optional) so missions without it are unaffected.

**New public surface:** `crate::telemetry::RecvTransport`; an uplink field on `TelemetryConfig`
(or a sibling `UplinkConfig`); the channel↔slot map is internal.

**Verify:**
- `cargo build -p metor-fsw-2` and `--no-default-features`.
- `cargo test -p metor-fsw-2` — a loopback test: a mock `RecvTransport` whose `recv` yields a
  `SequenceCommand::load(channel_id=0, "commissioning")` then `start`; build a coordinator with one
  slot started **empty**, drive `run_for(N)`, assert (via a `SlotStatus`/`SequenceStatus` tap, the
  `sequences.rs` pattern) the addressed slot reaches `Loaded`→`Running` — and that it lands the
  **same cycle** the command is delivered (no off-by-one).
- **Regression gate:** `cargo test -p metor-fsw-2 telemetry::` (downlink unaffected by the broker
  restructure — connect-once/no-reconnect/silent-drop preserved).

---

## Wave 4 — the sequence coupling (`src/coordinator/slot.rs`, `src/coordinator/mod.rs`)

**Depends on W1** (the `MsgOut` port) and the existing `SlotRunner`. The coordinator emits the
boot `SequenceRegistry`; the `SlotRunner` emits `SequenceChannelEvent`s host-side at each
transition and derives `Progress`/terminal outcome from its occupant's `SequenceStatus`
(`docs/messages.md` §5). Ungated. Uses `metor_proto_wkt` directly (already re-exported,
`src/lib.rs:103`).

1. **Boot `SequenceRegistry`** (`src/coordinator/mod.rs`, at `build()` / first `run_for`): the
   coordinator mints a `MsgOut` over a coordinator-owned message ring (the `msg_writer` helper, W1)
   and emits one `SequenceRegistry { channels }` where each `SequenceChannelSpec { id, name,
   available }` = `(channel↔slot map id (W3), slot name, the slot's allowed-occupant names)`. The
   allowed names come straight off `SlotReg.allowed[*].name` (`src/coordinator/slot.rs:243,278`).
   Emit once at startup (and leave a re-emit hook for a future `ReloadSequences`, deferred). Register
   that coordinator message ring in the `MessageRegistry` (W1) so telemetry taps it.
2. **`SlotRunner` gains a `MsgOut` + an occupant `SequenceStatus` view**
   (`src/coordinator/slot.rs:293-320`):
   - A per-slot **`events: MsgOut<BoxBacking>`** over a per-slot message ring (one writer per ring
     — single-writer discipline; allocate it in the slot-aux loop beside the control/status rings,
     `src/coordinator/mod.rs:891-926`, and register it in the `MessageRegistry`).
   - An **`Input<SequenceStatus>` view on the slot's own `SequenceStatus` output ring** — that ring
     is already allocated + registry-tapped (`src/coordinator/mod.rs:903`, the occupant's
     `SequenceStatus` output, `src/sequence/mod.rs:261`). The `SlotRunner` claims a `View` on it at
     build (one extra reader slot, covered by `READER_SLACK`).
   - The slot’s `channel_id` (its build-order index, Q2) stored on the runner for the event payload.
3. **Emit events at each transition** (`src/coordinator/slot.rs`, in the existing `do_*` methods):
   `do_load(name)` `:385` → `Loaded { name }`; `do_start` `:404` → `Started`; `do_stop` `:413` →
   `Stopped`; `do_reset` `:435` → `Loaded { name }` (re-arm to idle); `do_unload` `:449` →
   `Unloaded`. Each is `self.events.emit(&SequenceChannelEvent { channel_id, kind })` at the
   transition point — **every-record**, never diffed from the latest-wins `SlotStatus` (two
   commands can apply in one drain, so a snapshot would lose `Loaded`; `docs/messages.md` §5.1).
4. **Derive `Progress` + terminal outcome from `SequenceStatus`** (`src/coordinator/slot.rs`, in
   `step` `:487`, each cycle while `Running`): **drain every record** of the occupant
   `SequenceStatus` view (`Input::drain`, `src/port.rs:229`); for each, read the `progress`
   `FrameList` lines (`src/sequence/mod.rs:266`) and `emit` a `Progress { detail }` per **new**
   line; track `run_state`. When `step` folds `FswStatus::Done` (`slot.rs:499`, currently
   `Done{outcome:0}` — refine using the last `SequenceStatus.run_state`, `src/sequence/mod.rs:110`):
   `run_state==1`→`Completed`, `==2`→`Aborted`, `==3`→`Failed{reason}`; `FswStatus::StoppedLapped`/
   `Panicked` (`slot.rs:500-505`) → `Failed { reason: "lapped"/"panicked" }`.
5. **Mapping table** (the precise `SlotPhase`/`SequenceStatus` → wkt, `docs/messages.md` §5.2) —
   implement exactly as documented; `SequenceRunState` (`msgs.rs:692`) is used only if a
   channel-snapshot Msg is added (not required for v1; events suffice).

**`Failed { reason }` wrinkle (documented, accept the generic reason):** `SequenceStatus` carries
only `run_state`, no reason string (`src/sequence/mod.rs:261`). v1 emits a generic
`"failed"`/`"lapped"`/`"panicked"`. A real reason needs a new `SequenceStatus` field — **future
work**, called out so the planner does not try to thread a reason that doesn’t exist.

**New public surface:** none required externally (the coupling is coordinator-internal); the
emitted Msgs are `metor_proto_wkt` types already public.

**Verify:**
- `cargo build -p metor-fsw-2` **and** `cargo build -p metor-fsw-2 --no-default-features` —
  **the wkt-gating check (R3):** confirm `SequenceRegistry`/`SequenceChannelEvent` compile in the
  coupling under `--no-default-features` (they should: `metor-proto-wkt` is non-optional with
  `nox,std`, `Cargo.toml:15`, and these types are not feature-gated, `msgs.rs:681,725`).
- `cargo test -p metor-fsw-2 coordinator::` — a slot-unit test (no KDL): build a coordinator with
  one slot + the in-proc sequence path, tap the slot’s message ring via the `MessageRegistry`,
  drive `control_handle` `Load`→`Start`, step under a `Simulated` clock, and assert the drained
  Msgs are `Loaded`→`Started`→`Progress*`→`Completed` **in order** (every-record, no coalescing) and
  that the boot `SequenceRegistry` lists the slot with its `available` set.

---

## Wave 5 — example + end-to-end (`examples/adcs-fsw2`)

**Depends on W2 + W3 + W4.** The `adcs-fsw2` mission already has the `mode` slot with
`commissioning`/`safe_mode` occupants (`examples/adcs-fsw2/mission.kdl`) and a
`tests/sequences.rs` that taps `SequenceStatus`/`ModeCmd` and drives the slot via
`control_handle` (`tests/sequences.rs:63,151-176`). This wave proves the wire path end-to-end.

1. **Downlink assertion test** (extend `examples/adcs-fsw2/tests/sequences.rs` or a new
   `tests/sequence_messages.rs`): build the mission with a `MockTransport`-style downlink (reuse
   the `src/telemetry/tests.rs` `MockTransport` pattern, or a loopback `TcpTransport` to an
   in-process listener), `run_for` under the `Simulated` clock with `commissioning` auto-running
   (mission default, `mission.kdl` `initial … state="running"`), and assert the captured wire
   packets contain (a) a `SequenceRegistry` listing the `mode` channel with `available =
   ["commissioning","safe_mode"]`, and (b) the `commissioning` channel's `SequenceChannelEvent`s
   `Loaded`→`Started`→`Progress`→`Completed` (parse each captured `LenPacket` via
   `OwnedPacket`/`MsgBuf`). This is the exact gap the panel investigation found — the sequence view
   now populates.
2. **Uplink drive test:** start the `mode` slot **empty** (the interactive scenario,
   `tests/sequences.rs:151`), inject a `SequenceCommand::load(channel_id=0, "commissioning")` +
   `start` through the **uplink** (a mock `RecvTransport`, W3) instead of `control_handle`, and
   assert the slot loads/starts and the resulting events come back on the downlink — the full
   panel round-trip (command in → events out).
3. **No new cdylibs needed** — the sequence occupants and the slot already exist; this wave is
   tests + (if the example’s telemetry harness needs it) a small downlink/uplink setup helper.

**Verify:**
- `cargo test -p adcs-fsw2` — the new downlink + uplink tests pass; the existing
  `commissioning_auto_runs_to_completion` / `interactive_load_then_abort_safes`
  (`tests/sequences.rs:107,151`), `closed_loop.rs`, and `bundle.rs` stay green (the feature is
  additive — the slot’s `mode_cmd` component path is unchanged).
- Final multi-crate gate: `cargo build -p metor-fsw-2 && cargo build -p metor-fsw-2
  --no-default-features && cargo test -p metor-fsw-2 && cargo test -p adcs-fsw2`. Then commit (task
  boundary).

---

## Notes, invariants, and open implementation risks

**Risks to decide / watch during impl:**

- **R1 — the lazy-connect → shared-socket broker restructure (W3, HIGHEST). DECIDE the broker
  shape.** Today the socket is established **lazily inside the sender task** (`ensure`,
  `src/telemetry/mod.rs:97`); the uplink reader needs the **same** connection’s read half, so
  establishment must move to a shared point that splits the stream (`docs/messages.md` Q7, resolved
  to one shared connection). The restructure touches the downlink's connection path — the regression
  hazard is breaking **connect-once / no-reconnect / silent-drop-on-disconnect** (telemetry.md §7).
  *Recommendation:* a `connect_once()` broker that does the single `split()` and hands the sink to
  `run_sender` and the stream to `run_receiver`, both spawned from telemetry `init`; keep the
  Mock/test path able to supply both halves without a socket; add **no** reconnection. *Decision
  wanted before coding:* (a) is moving establishment from "first announce" to a shared
  init/first-use point acceptable, and (b) should the broker be a new `Connection` type both tasks
  share, or should `TcpTransport` keep ownership and expose its already-held `rx` half
  (`telemetry/mod.rs:76`) to the reader? I lean (a) new `Connection` broker for a clean split, but
  flag it — it is the one structural change to a shipped, tested subsystem.

- **R2 — the non-coalescing message hand-off (W2, watch).** `MsgHandOff` must be a real FIFO
  (every record, in order), **not** the latest-wins `Vec<Option<LenPacket>>` of the component
  `HandOff` (`src/telemetry/mod.rs:188`). *Decision:* one shared bounded FIFO across all message
  taps (recommended — cross-channel order is irrelevant, each Msg self-addresses) vs per-tap
  queues; and the overflow cap + policy (recommend cap ~1024, drop-oldest + `msg_dropped` counter).
  Low risk, but the whole point of the feature is *not dropping/reordering events*, so get the
  drain-every-record (`Input::drain`, not latest-wins) right and test N-in-N-out.

- **R3 — wkt under `--no-default-features` (W4, verify).** The coupling uses
  `SequenceRegistry`/`SequenceChannelEvent`/`SequenceCommand` directly. They should compile without
  the crate's `kdl` default (wkt is non-optional `nox,std`, `Cargo.toml:15`; the types are
  ungated, `msgs.rs:681,725,753`) — but **assert it explicitly** in the W4 verify step, since the
  whole message/coupling surface must stay in the `--no-default-features` build.

- **R4 — channel_id stability + W3/W4 consistency (watch).** `channel_id` = the slot's build-order
  index (Q2, locked) — not stable across wiring edits, but per-session panel state makes that fine.
  The **one consistency invariant:** the index W3’s uplink uses to address a command and the index
  W4’s `SequenceRegistry`/events publish **must be the same** map. Build it **once** in the
  coordinator (W3, the channel↔slot map) and have W4 read it — do not compute the index twice.

**Invariants:**
- The entire surface (message channel, downlink tap, uplink, sequence coupling) is **ungated** —
  no `kdl`. `cargo build -p metor-fsw-2 --no-default-features` must build it at every wave.
- **Zero ABI change** (`docs/messages.md` §6): emit is host-side (`SlotRunner`/coordinator over
  `BoxBacking`), the uplink is host-side; the occupant, `fsw_*`, and `FSW_ABI_VERSION` are
  untouched.
- Messages are **self-describing**: telemetry sends `LenPacket::msg(id, …)` with **no announce**
  (contrast the component `VTableMsg` announce, `src/telemetry/mod.rs:63,398`). Events are an
  every-record **log**, never coalesced like a component snapshot.
- The uplink reuses the **existing** slot command ring + `drain_commands` dispatch
  (`src/coordinator/mod.rs:931,1411`) — it only *writes* `SlotCommand`s at the head of the cycle;
  it adds no new slot-dispatch path.
