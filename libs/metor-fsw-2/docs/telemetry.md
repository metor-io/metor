# Telemetry Downlink + the Registry

The telemetry downlink streams other systems' outputs — component frames *and* message channels
alike — out of the process in metor-proto's wire format — the format `metor-db` ingests. It rests
on a general capability, the **registry** (`src/registry.rs`, one keyspace for both payload kinds),
which lets any broad or dynamic reader reach every tappable buffer in the graph by id. The downlink
is the registry's first consumer; a logger, recorder, or debugger would use the same registry the
same way.

The code lives in two modules:

- `src/registry.rs` — `Registry` and `RegistryEntry`: the by-id index over the coordinator's
  ring table.
- `src/telemetry/mod.rs` — `TelemetrySystem`, the `Transport` trait and `TcpTransport`, the
  snapshot/hand-off mechanism, the async sender. Unit tests are in `src/telemetry/tests.rs`.

---

## 1. Purpose & the metor-proto wire symmetry

Flight software is useful only if its state can be observed off-board. The downlink **taps** the
output ring buffers that other systems already write and re-emits each buffer's latest record as a
metor-proto **`Table` packet** referencing a once-announced **`VTable`**. A frame's ring payload *is*
its table bytes — `Output::write` does a single `try_write` of `frame.as_bytes()` — so there is no
serialization step: the bytes a system committed are the bytes that go on the wire, and `metor-db`
stores them with no translation. The downlink and the database are two ends of one protocol.

This is the same VTable symmetry that `examples/cube-sat/src/main.rs` exercises by hand: it calls
`tx.init_world::<CubeSat>(id)` once (vtable + metadata) and then, every cycle,
`LenPacket::table(id, cap)` / `extend_from_slice(frame.as_bytes())` / `tx.send(pkt)` / `pkt.clear()`.
The downlink generalizes that hand-written loop to *every* output of a running graph, with
per-instance namespacing, without blocking the control cycle on the socket.

---

## 2. The general registry

The mechanism the streamer uses to reach all outputs is a first-class capability available to every
system, not a telemetry special case. The registry is the centerpiece; telemetry is merely its first
consumer.

### 2.1 What it is

The coordinator owns a `RingTable`: a `Vec<RingEntry>` where every `RingEntry { ring, frame_id, role,
instance }` is one buffer in the graph. The registry is a thin, queryable index over that same table.
It is built in `build()` from the fully populated ring table and stored as `Arc<Registry>` on
the `Coordinator`; `Coordinator::registry()` returns a clone of that `Arc`.

```text
Registry
  entries:  Vec<RegistryEntry>           // one per tappable buffer, build order
  by_key:   HashMap<ComponentId, usize>  // instance-qualified id -> entries index
```

```text
RegistryEntry
  key:         ComponentId    // instance-qualified id  (§2.2)
  instance:    Arc<str>       // "imu_left", or "coordinator" for coordinator-owned buffers
  name:        Arc<str>       // the port name within the instance: F::NAME, M::NAME, or an
                               // explicit coordinator channel string ("sequences")
  schema:      EntrySchema    // Table { frame_id, vtable, metadata } | Postcard   (§6)
  delivery:    Delivery       // Snapshot | Log — how a broad reader should drain this entry
  telemetered: bool           // does the downlink / AllOutputs tap this entry (§3)
  ring:        RingBuffer<BoxBacking>  // crate-private read source; reached only via view()
```

**One registry, both payload kinds.** The registry indexes every tappable buffer — component
frames *and* message channels alike, including the coordinator's own `commands`/`sequences`
channels — in one keyspace, so a same-instance name collision between a frame and a channel is
*detectable* at `build()` (`WireError::DuplicateRegistryKey`) rather than shadowed by two parallel
tables. `EntrySchema` is the registry's projection of the port's `PortSchema` axis (system.md §5.1):
`Table { frame_id, vtable, metadata }` carries the **prefixed** announce schema (§6, captured at
`build()` — the authoritative unprefixed `VTable` lives on `PortDesc` and is otherwise dropped after
sizing, because a broad consumer reads buffers by id and has no static frame type `F` to call
`F::as_vtable()` on); `Postcard` carries neither — a `(PacketId, postcard)` record is
self-describing on the wire, so a message entry has no vtable/metadata to capture. `telemetered`
lets an entry stay registered (visible to a debugger/test by key through the full `Registry`) while
being filtered out of the in-graph `AllOutputs` broadcast tap — this is how a command channel (e.g.
the uplink's or coordinator's `commands`) avoids echoing straight back onto the downlink (§3)
without needing a second index.

`RegistryEntry.ring` is crate-private. External callers reach the buffer only through
`RegistryEntry::view()`, so every reader is slot-accounted (§2.5); the registry never hands out the
raw `RingBuffer`.

### 2.2 The key

Two instances of one system type share a frame `ComponentId` (`ImuDriver`'s `imu` frame is
`ComponentId::new("imu")` for *both* `imu_left` and `imu_right`), so `frame_id` alone cannot be the
key. The key is the **instance-qualified id** `ComponentId::new("<instance>.<frame>")` — e.g.
`ComponentId::new("imu_left.imu")`.

- It is a `ComponentId` (a `u64`): cheap, `Copy`, `Hash`, the project's universal name type.
- **It is exactly the value the downlink puts on the wire** — the prefixed `Op::Frame` tag id (§6).
  The key, the wire id, and the prefix are one identity.
- It is derivable from data the `RingEntry` already carries: `instance` + `frame_id`.

`instance` (`Arc<str>`) and `frame_id` are kept on the entry for human-readable subset filtering
(§3) and for the metadata names, but the map key is the qualified id.

### 2.3 Build order

The registry is built in `build()` before the bind loop, so a system can pull it in
`BindPorts::bind`. To make this work, the per-system output rings *and* the coordinator-owned
`health`/`log`/`coordinator_status` rings are allocated up front (the coordinator-owned rings depend
on no edges), the registry is assembled from all of them, and only then does binding run. Each
`RegistryEntry` is built once at allocation, capturing the prefixed announce vtable + metadata (§6).

### 2.4 How a system gets a handle

A registry consumer receives `Arc<Registry>` through the binder. The host `Binder` carries the
registry and exposes `RingSource::registry(&self) -> Arc<Registry>`. A system whose output bundle
wants broad access declares a field of the **`AllOutputs`** capability type (`src/registry.rs`) —
not a typed port — in its output bundle; the `#[derive(SystemOutput)]` walk sees `AllOutputs::decl()`
contribute `PortDecl::Capability(Capability::ReceiveAll)` instead of a `PortDesc`, and
`AllOutputs::bind` pulls the registry handle in the generated `BindPorts::bind`, exactly where typed
ports pull their rings. Because the registry is complete before the bind loop (§2.3), this is safe
and needs no second phase.

The registry coexists with typed `connect`/`PortRef` wiring cleanly: **typed wiring is for known
compile-time edges** (validated, compatibility-checked, sized into fan-out); **the registry is for
broad or dynamic access** where the consumer does not know the producer at compile time. They share
the same underlying rings; the registry never bypasses or duplicates a typed edge, it offers a by-id
read path over the same ring table.

Registry access is host-only: `AllOutputs::bind` is implemented for `B = BoxBacking` only — a
non-host `RingSource`'s default `registry()` panics rather than fabricate an empty one (system.md
§5.4). The telemetry downlink is never dlopen'd.

### 2.5 Sizing: every registry reader is a fan-out consumer

`RegistryEntry::view()` calls `RingBuffer::view(NoWake, NoWake)`, which **claims a reader slot** from
the buffer's fixed `max_readers` table. This is the critical interaction with build-time sizing: the
rings have no crash-slot reclamation, so `max_readers` is set once at `build()`.

The coordinator sizes for the known registry consumers **self-derived, not manually bumped**:
`build()` counts how many descriptors declare the `ReceiveAll` capability (`n_reg`) by scanning
every registered system's `capabilities` — `add_telemetry` needs no counter-bump call, and any
future `AllOutputs`-declaring system (a recorder, a second downlink) is picked up automatically just
by declaring the field. Every output ring is sized

```text
max_readers = fan_out + n_reg + READER_SLACK   // READER_SLACK = 4
```

and the coordinator-owned buffers are sized `1 + n_reg + READER_SLACK`. "All"-mode telemetry
contributes one slot to every buffer (one `AllOutputs` field); a second registry consumer would add
one more each. A cyclic system declaring `ReceiveAll` must additionally be **registered last**
among cyclic systems (`WireError::ReceiveAllNotLast`, enforced at `build()`, coordinator.md §3.4) —
its end-of-cycle snapshot would otherwise observe some systems one cycle stale.
This is exact and cannot over-subscribe for the known consumers.

If a slot budget is nonetheless exhausted — a hand-built over-subscription — `view()` returns
`Err(FullReaderTable)`; the downlink surfaces it as health and skips that tap rather than panicking
(§3). There is no runtime late attach: a consumer connecting after `build()` has no reserved slot
beyond the static `READER_SLACK`.

---

## 3. Telemetry as a registry consumer

`TelemetrySystem<T: Transport>` is an ordinary `CyclicSystem`, **registered last** so its `execute`
runs after every other system's `execute` in the cycle (`run_for` drives the cyclic systems in
registration order) — an end-of-cycle snapshot of the freshest output of each tapped buffer.

It has no typed input ports. `TelemetryIn` is empty; `TelemetryPorts` (its output bundle) declares
one `AllOutputs` field — a bind-time `Capability`, not a wired port (system.md §5.3) — which exists
only to carry the `Arc<Registry>` pulled via the binder (§2.4). `AllOutputs::entries()` is already
telemetered-only (the `Registry`'s `telemetered` flag filters at the source), so a command channel
or an opted-out frame never even reaches the mode matcher below.

**`TelemetryMode`** selects the tap set, over both payload kinds uniformly:

- `All` — tap every telemetered registry entry, frames *and* message channels alike. This includes
  every system's user output frames *and* their implicit per-system `health`/`log` frames (those are
  `Output`-role buffers carrying the system's instance), plus the coordinator-owned
  `health`/`log`/`coordinator_status`/`sequences` buffers (prefixed `"coordinator"`, §6) — but *not*
  an untelemetered channel (e.g. `coordinator.commands`, the uplink's `CommandOut` outputs).
- `Subset { instances, frames }` — tap an entry when its `instance` name matches one of `instances`
  **or** its `name` (`RegistryEntry::name` — a frame's `F::NAME` or a message channel's `M::NAME`)
  matches one of `frames` (a plain string compare — the field is called `frames` for historical
  reasons but matches a message channel's name identically). Matching either list is enough.

**`init()`** resolves the tap set and spawns the sender. It runs on the coordinator's loop task
within `start()`, so `stellarator::spawn` has a runtime and the sender announces before any data is
queued. For each registry entry that `mode.matches`, `init`:

- claims one read `View` via `entry.view()`; on `Err(FullReaderTable)` it records
  `output.health().error("telemetry.reader_slot")` and skips the tap;
- picks a **lane** off the entry's `Delivery` axis (Snapshot → a coalescing slot, Log → the shared
  FIFO, §4) and a **wire** framing off its `EntrySchema` (Table → announce + a sequential `PacketId`,
  Postcard → forwarded as-is, no announce);
- records a `Tap { view, lane, wire, last_committed }` and, for a Table entry, an
  `Announce { packet_id, vtable, metadata }` carrying the entry's prefixed schema.

**Known gap (B9): the init-time emit window.** `RingBuffer::view()` starts a reader **at the
buffer's current commit point** — "it never sees older data" (`ring/src/lib.rs`).
Every other system's `init` (both async systems, behind the coordinator's startup barrier, and every
cyclic system, in registration order) completes **before** telemetry's own `init` runs (telemetry is
registered last, coordinator.md §3.4/§3.7), so any frame or message a system emits *only* during its
own `init` — and never rewrites later — is already committed by the time telemetry's `view()`
attaches, and is therefore invisible to the downlink for the rest of the run: the view's cursor
starts right past it. A frame that is republished every cycle (the common case) is unaffected — the
downlink simply picks it up the next cycle. This applies symmetrically to a live panel connecting
*after* the mission has been running for a while: a fresh telemetry view (and a fresh panel
subscription behind it) starts at whatever the ring's live edge is at attach time, not at mission
start, so a one-shot init-time record is equally invisible to a late-joining consumer. Not yet fixed;
noted here as the honest limit of "the downlink taps every output," not solved by a backlog replay.

It then builds the hand-off (§4) sized to the tap count, an `AtomicBool` stop flag, and spawns
`run_sender`, retaining the join handle as a `JoinHandleDropGuard` (its `Drop` cancels the task at
teardown).

**`execute(now)`** (end-of-cycle) drains each tap per its **lane** (§4) — `Coalesce` taps to their
newest record, `Fifo` taps every record in order — and surfaces any drops from either lane as
health (`telemetry.dropped` for coalesced snapshots, `telemetry.msg_dropped` for FIFO records).

**`shutdown()`** sets the stop flag and wakes the sender so it exits cooperatively; the drop guard
cancels it regardless when the system is dropped.

No telemetry-specific coordinator hook exists: the only coordinator surface is the general registry
(§2) plus the fact that any last-registered cyclic system observes end-of-cycle state. A recorder or
logger would be written identically.

---

## 4. Cycle / sender split — never block control on the network

The cycle is single-threaded and synchronous; `PacketSink::send` is async TCP. `execute` must not
await the socket — a slow or stalled link must never delay control. The work is split between the
in-cycle drain stage and an async sender task, bridged by one bounded, **two-lane** hand-off.

Each tap is one of two lanes, chosen once at `init` from the entry's `Delivery` axis (§2.5) — this
is also where frames and messages stop needing separate machinery: a Table×Snapshot frame tap and a
Postcard×Log message tap are just two lane/wire combinations of the same `Tap`, and a hypothetical
Table×Log (an every-record frame log) falls out for free with zero extra code.

### Drain stage (in-cycle, `execute`)

For each tap, `execute` reads the `View` per its lane, borrowing each record **in place** off the
ring — there is no per-tap scratch copy:

```text
Lane::Coalesce { slot }:      // Snapshot entries (frames, by default)
  compare the ring's `committed` word against the tap's `last_committed`; if
  unchanged, nothing new this cycle — skip (the pinned newest record is not
  re-sent). Otherwise borrow the newest record via View::try_latest, build one
  LenPacket (Table or Msg framing, per the tap's `wire`) and
  handoff.push_snapshot(slot, pkt)     // never blocks

Lane::Fifo:                   // Log entries (messages, by default)
  drain every record in order (per-record read grants); build one LenPacket per
  record and
  handoff.push_log(pkt)                // never blocks, for each record
```

A buffer with no new record this cycle is simply skipped. Each record is one copy: from the borrowed
ring bytes into the freshly built `LenPacket`. It is a `memcpy`; there are no syscalls and no
`.await` in the cycle.

### Hand-off (`HandOff`) — one struct, two lanes

```text
HandOff
  slots:             Mutex<Vec<Option<LenPacket>>>  // Snapshot lane: one coalescing slot per tap
  fifo:               Mutex<VecDeque<LenPacket>>     // Log lane: one bounded FIFO shared by every Log tap
  pending:            AtomicBool                     // either lane has something waiting; avoids busy-spin
  dropped_snapshots:  AtomicU64                      // Snapshot lane: coalesced-away (overwritten un-sent)
  dropped_logs:       AtomicU64                      // Log lane: dropped-oldest on overflow
  wq:                 WaitQueue                      // wakes the parked sender
```

- **Snapshot lane** — `push_snapshot(slot, pkt)` (cycle side, never blocks): if the slot is already
  occupied, the previous un-sent packet is overwritten and `dropped_snapshots` is incremented; the
  new packet takes the slot. A snapshot is latest-wins state, so a newer one supersedes an older
  un-sent one — at most one pending packet per tap.
- **Log lane** — `push_log(pkt)` (cycle side, never blocks): appended to a shared, bounded FIFO
  (`LOG_HANDOFF_CAP = 1024`); an event/command record must never be coalesced, so every drained
  record is queued in order and forwarded verbatim (cross-*channel* order is irrelevant — each
  record self-addresses by its wire id). Overflow drops the **oldest** queued record (not the
  newest) and counts it in `dropped_logs`, bounding memory while keeping the most recent history.
- Both `push_*` set `pending` and wake the sender.
- `drain()` (sender side): takes every occupied Snapshot slot *and* the whole Log FIFO in one call
  — `(Vec<LenPacket>, Vec<LenPacket>)` — releasing both locks before any `.await`.

### Sender task (`run_sender`)

`stellarator::spawn`ed at `init`. It first announces every tap once (§5); if any announce fails it
exits. Then it loops: if the stop flag is set it returns; otherwise it drains the hand-off and sends
each packet via `Transport::send`. When the hand-off is empty it parks on the wait queue until
`pending` is set or stop is signalled. Any transport error stops downlinking — the task returns and
the cycle is unaffected.

### Drop policy

The rings themselves are lossless — a writer backpressures rather than overwrite unread data — so
the hand-off is where the "never let a slow link touch the cycle" trade is made explicitly:

- The Snapshot lane coalesces per tap; the cycle never blocks on a backed-up link.
- When a stalled link leaves a Snapshot slot occupied, the next snapshot overwrites it and counts a
  drop; the Log lane drops the **oldest** queued record past its cap instead — losing history, never
  reordering the record log by dropping a newer one.
- `execute` surfaces both in-band: it compares `HandOff.dropped_snapshots`/`dropped_logs` against
  `last_dropped`/`last_msg_dropped` watermarks and emits `output.health().error("telemetry.dropped")`
  / `.error("telemetry.msg_dropped")` once per newly dropped record, so loss is observable through
  the telemetry system's own health frame.

---

## 5. Wire protocol — announce once, stream per cycle

The downlink reuses the metor-proto primitives verbatim; nothing here is new protocol.

- **Announce-once (per Table tap, on connect).** Each Table tap has a sequential `PacketId`
  (`[u8;2]`). The sender sends `VTableMsg { id, vtable }` carrying the **prefixed** vtable (§6),
  followed by one `SetComponentMetadata(ComponentMetadata)` per component. This is exactly what
  `SinkExt::init_world` does (`send_vtable` then `send_metadata`), but with a per-instance prefix
  instead of the type's static names. A **Postcard** (message) tap needs no announce — the record is
  self-describing on the wire (its own `PacketId` prefixes the postcard payload), so `wire = Wire::Msg`
  skips this step entirely (§4).
- **Stream-per-cycle.** For a Table tap with a fresh record, `execute` builds `LenPacket::table(id,
  cap)` and `extend_from_slice`s the latest table bytes; for a Postcard tap it builds
  `LenPacket::msg(id, cap)` from the record's own embedded id instead. Either way the sender forwards
  it verbatim; `metor-db` ingest receives `VTableMsg` then `Table(id)` packets for frames, or bare
  `Msg` packets for messages — precisely what the downlink emits.
- **Dynamic frames** (`FrameList`/`FrameMap`). The table bytes already include the trailer
  (`Output::write_with`/`publish_with` writes `fw.table()`), and the announced `VTable` already
  carries the `Op::List`/`Op::Map` ops. The downlink forwards both as-is — bytes plus List/Map-bearing
  vtable — and does nothing dynamic-aware itself; it is a pure forwarder.

---

## 6. Instance-name prefixing

Each tapped buffer is announced under a vtable + metadata whose component names are prefixed by the
instance name, so `imu_left` emits `imu_left.imu.omega` and `imu_right` emits `imu_right.imu.omega` —
never colliding on the wire despite sharing the `imu` frame id. The **table bytes are unchanged**: the
layout is positional, and only the names/ids the vtable maps those bytes to change.

The unprefixed `VTable` on `PortDesc` bakes each component's *hashed* id into the ops, and a hash is
one-way, so the prefixed vtable cannot be re-derived from the unprefixed one. Instead the prefixed
schema is re-derived from the static frame type with the instance as a `ComponentPath` prefix:

- `AsVTable::vtable_fields(prefix)` takes a path prefix, so the prefixed vtable is
  `vtable(F::vtable_fields(prefix))` — leaves become `component(instance.chain(frame).chain(field).id)`.
- `Metadatatize::metadata(prefix)` likewise yields prefixed `ComponentMetadata`. The alloc-free
  `PathHasher` rolls the same id as `ComponentId::new("<instance>.<frame>.<field>")`.

Because this needs the static `F` (erased to `PortDesc` by build time), a Table `PortDesc`'s
`PortSchema::Table` variant carries the prefix factory captured when the descriptor is derived
(system.md §5.1):

```text
PortSchema::Table {
    vtable: VTable,
    announce: AnnounceFn,   // Arc<dyn Fn(&str) -> (VTable, Vec<ComponentMetadata>)>
}
```

`PortDesc::of::<F>()` stores `announce_of::<F>`, a closure that closes over `F` and produces the
prefixed vtable + metadata for any instance name. (A Postcard `PortDesc` carries no `announce` at
all — a message channel has no per-instance schema to prefix, only its `PacketId`.) At `build()` the
coordinator knows each port's instance, calls `announce(instance)`, and stores the result in the
entry's `EntrySchema::Table { vtable, metadata, .. }` (§2.1). Coordinator-owned buffers
(`instance = None`) use the synthetic prefix `"coordinator"`.

The prefixed `Op::Frame` tag id equals `ComponentId::new("<instance>.<frame>")` — the same value used
as the registry key (§2.2), so the key, the wire id, and the prefix are one consistent identity.

---

## 7. Transport abstraction

A small trait isolates the wire from the streamer:

```rust
pub trait Transport {
    async fn announce(&mut self, msg: &VTableMsg, meta: &[ComponentMetadata]) -> Result<(), TransportError>;
    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError>;
}
```

`TransportError` is either `Disconnected` or `Io(String)` (a rendered, transport-agnostic I/O error).
Any error stops downlinking; the in-cycle snapshot stage keeps running and simply drops.

**`TcpTransport`** is the shipping transport. It holds a `SocketAddr` and an optional `TcpConn`
(`PacketSink<OwnedWriter<TcpStream>>` plus the read half, held only to keep the socket open — replies
are not read). It connects lazily on first use inside the async sender task — `TcpStream::connect`,
`split()`, `PacketSink::new` — the same path cube-sat uses. `announce` is `send_vtable` +
`send_metadata` (a `VTableMsg` then a `SetComponentMetadata` per component); `send` is
`PacketSink::send`.

The system is generic over `T: Transport`. The builder/loader supplies a `TcpTransport`; the unit
tests drive a deterministic in-memory mock against the same trait.

**Lifecycle.** Connect once. On disconnect the sender task returns and stops downlinking; the in-cycle
snapshot stage keeps running and drops (its hand-off fills) so control is unaffected.

---

## 8. KDL / builder surface

The downlink is declared like any other node in a wiring document, plus a builder convenience.

```kdl
telemetry {
    transport "tcp" addr="127.0.0.1:2240"
    mode "all"                              // or: mode "subset"
    // subset only — taps these instances/frames:
    // tap instance="imu_left"
    // tap frame="control"
}
```

- The loader (`src/wiring/mod.rs`) parses the `telemetry` node into a `(addr, mode)` pair, validates
  that `transport` is `"tcp"` and `mode` is `"all"` or `"subset"`, and adds the downlink after every
  `system` node so it is registered last (§3).
- **Builder surface:** `CoordinatorBuilder::add_telemetry(TelemetryConfig { transport, mode })`. It is
  an ordinary `add_cyclic_named("telemetry", TelemetrySystem::new(config))` — no manual reader-slot
  bookkeeping: `TelemetryPorts`'s `AllOutputs` field is what earns the downlink its reader slot on
  every buffer, self-derived at `build()` by counting `ReceiveAll` capabilities (§2.5).
- `TelemetryConfig<T: Transport>` carries the concrete `transport` value and the `mode`.

---

## 9. Reused vs new

**Reused:**

- The coordinator's `RingTable`/`RingEntry`/`BufferRole` and `output_instances` — the registry indexes
  this, it does not replace it.
- `RingBuffer::view`, `View::try_latest`/`try_read` (`metor-fsw-ring`).
- `PortDesc.vtable` — the authoritative layout, captured into the registry entry.
- `VTableMsg`, `SetComponentMetadata`, `LenPacket::table`/`extend_from_slice`, `PacketSink::send`,
  `SinkExt::{send_vtable, send_metadata, init_world}` — the whole wire path; cube-sat is the precedent.
- `ComponentPath`/`chain`/`PathHasher`, `AsVTable::vtable_fields(prefix)`,
  `Metadatatize::metadata(prefix)` — the prefix.

**New:**

- `Registry` + `RegistryEntry`, keyed by the instance-qualified id, stored as `Arc<Registry>` on
  `Coordinator` and reachable via `Binder::registry()` — one index for frames *and* message channels
  (`EntrySchema::{Table, Postcard}`).
- The prefixed `vtable`/`metadata` on a Table entry, sourced via the `announce` prefix factory on
  `PortSchema::Table`.
- `max_readers = fan_out + n_reg + READER_SLACK` sizing, `n_reg` self-derived by counting
  `ReceiveAll` capabilities across every registered descriptor.
- `TelemetrySystem`, the two-lane (Coalesce/Fifo) drain → hand-off → async-sender split, the
  per-lane drop policy with its `telemetry.dropped`/`telemetry.msg_dropped`/`telemetry.reader_slot`
  health counters, and the `Transport` trait with `TcpTransport`.

---

## 10. Not yet implemented

- **`bbq`/SHM transport.** `metor-proto/bbq` is a local shared-memory framed queue speaking the
  identical `LenPacket` format and would be a drop-in `Transport` for a co-located consumer. Only the
  TCP transport ships; the KDL loader accepts `transport "tcp"` only.
- **Automatic reconnect/backoff.** The TCP transport connects once and stops downlinking on
  disconnect.
- **Runtime late attach.** Broad access is sized for the registry consumers known at `build()`; the
  rings have no crash-slot reclamation, so a consumer attaching at runtime has no reserved slot beyond
  the static `READER_SLACK` (§2.5).
